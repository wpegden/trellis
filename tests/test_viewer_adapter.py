from __future__ import annotations

import json
from pathlib import Path

from trellis.viewer_adapter import (
    GitSnapshot,
    _awaiting_human_input,
    _bridge_corr_issues,
    _extract_last_review,
    _live_chats,
    _historical_activity_map,
    _historical_open_blockers,
    _meta_source,
)


def test_bridge_corr_issues_uses_normalized_node_specific_evidence() -> None:
    payload = {
        "normalized": {
            "node_lane_updates": {
                "v1": {"A": {"Set": "Pass"}},
                "v2": {
                    "A": {"Set": "Fail"},
                    "B": {"Set": "Fail"},
                },
            },
            "reviewer_evidence": {
                "v2": {
                    "correspondence": {
                        "decision": "FAIL",
                        "issues": [
                            {"node": "A", "description": "A issue"},
                            {"node": "B", "description": "B issue"},
                        ],
                    },
                    "summary": "lane summary",
                    "comments": "lane comments",
                }
            },
        }
    }

    issues = _bridge_corr_issues(payload)

    assert "A issue" in issues["A"]
    assert "B issue" in issues["B"]
    assert "lane summary" in issues["A"]
    assert "lane comments" in issues["B"]


def test_historical_activity_and_blockers_come_from_latest_artifacts(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    tablet = repo / "Tablet"
    tablet.mkdir(parents=True)
    (tablet / "Preamble.lean").write_text("import Mathlib\n", encoding="utf-8")
    (tablet / "AlternatingBinomialSum.lean").write_text("import Tablet.Preamble\n", encoding="utf-8")
    (tablet / "FactorialMomentIsolated.lean").write_text("import Tablet.Preamble\n", encoding="utf-8")
    snapshot = GitSnapshot(repo_path=repo, ref="HEAD")
    protocol_state = {"live": {"present_nodes": ["Preamble", "AlternatingBinomialSum", "FactorialMomentIsolated"]}}
    bridge_state = {
        "latest_corr.json": {
            "normalized": {
                "node_lane_updates": {
                    "v1": {"FactorialMomentIsolated": {"Set": "Pass"}},
                    "v2": {"FactorialMomentIsolated": {"Set": "Fail"}},
                }
            }
        },
        "latest_review.json": {
            "response": {
                "next_active": "AlternatingBinomialSum",
                "task_blockers": [
                    {
                        "kind": "NodeCorr",
                        "object": {"otype": "node", "node": "AlternatingBinomialSum"},
                    },
                ],
                "reset_blockers": [
                    {
                        "kind": "NodeCorr",
                        "object": {"otype": "node", "node": "FactorialMomentIsolated"},
                    },
                ],
            }
        },
    }

    activity = _historical_activity_map(
        snapshot=snapshot,
        protocol_state=protocol_state,
        bridge_state=bridge_state,
    )
    blockers = _historical_open_blockers(protocol_state, bridge_state)

    assert activity["FactorialMomentIsolated"]["correspondence"] is True
    assert activity["AlternatingBinomialSum"]["reviewer"] is True
    assert activity["FactorialMomentIsolated"]["reviewer"] is False
    assert blockers == ["NodeCorr:AlternatingBinomialSum", "NodeCorr:FactorialMomentIsolated"]


def test_human_gate_viewer_adapter_normalizes_live_kind_and_review_decision() -> None:
    protocol_state = {
        "stage": "HumanGate",
        "in_flight_request": {"kind": "HumanGate"},
    }
    review_payload = {
        "response": {"decision": "AdvancePhase"},
        "raw": {"reason": "looks good"},
    }

    assert _awaiting_human_input(protocol_state) is True
    assert _meta_source(protocol_state) == "cycle"
    assert _extract_last_review(review_payload) == {
        "decision": "advance_phase",
        "reason": "looks good",
    }


def test_live_chats_parse_transcript_files_into_compact_entries(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    state_dir = repo / ".trellis" / "chats" / "live" / "trellis_worker_69_result"
    state_dir.mkdir(parents=True)
    (repo / "trellis.config.json").write_text("{}", encoding="utf-8")
    (state_dir / "prompt.txt").write_text("Prompt body", encoding="utf-8")
    (state_dir / "transcript.jsonl").write_text(
        "\n".join(
            [
                json.dumps({"message": {"role": "user", "content": [{"text": "first prompt"}]}}),
                json.dumps({"message": {"role": "assistant", "content": [{"text": "assistant reply"}]}}),
            ]
        ),
        encoding="utf-8",
    )

    payload = _live_chats(repo, 29)

    assert payload["cycle"] == 29
    assert payload["source"] == "live"
    assert [artifact["id"] for artifact in payload["artifacts"]] == ["trellis_worker_69_result"]
    entries = payload["artifacts"][0]["entries"]
    assert entries[0] == {
        "role": "prompt",
        "kind": "prompt",
        "title": "Prompt",
        "text": "Prompt body",
    }
    assert entries[1] == {
        "role": "user",
        "kind": "message",
        "title": "user",
        "text": "first prompt",
    }
    assert entries[2] == {
        "role": "assistant",
        "kind": "message",
        "title": "assistant",
        "text": "assistant reply",
    }


def test_live_chats_filter_to_active_cycle_artifacts(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    (repo / "trellis.config.json").write_text("{}", encoding="utf-8")
    live_root = repo / ".trellis" / "chats" / "live"
    worker_68 = live_root / "trellis_worker_68_result"
    worker_69 = live_root / "trellis_worker_69_result"
    worker_68.mkdir(parents=True)
    worker_69.mkdir(parents=True)
    (worker_68 / "prompt.txt").write_text("old cycle", encoding="utf-8")
    (worker_69 / "prompt.txt").write_text("current cycle", encoding="utf-8")

    runtime_root = repo / ".trellis" / "runtime" / "test-runtime"
    runtime_root.mkdir(parents=True)
    (runtime_root / "runtime_metadata.json").write_text(
        json.dumps({
            "repo_path": str(repo),
            "config_path": str(repo / "trellis.config.json"),
        }),
        encoding="utf-8",
    )
    (runtime_root / "protocol_state.json").write_text("{}", encoding="utf-8")
    (runtime_root / "event_log.jsonl").write_text(
        "\n".join(
            [
                json.dumps({"commands": [{"command": "issue_request", "request": {"id": 68, "cycle": 28}}]}),
                json.dumps({"commands": [{"command": "issue_request", "request": {"id": 69, "cycle": 29}}]}),
            ]
        )
        + "\n",
        encoding="utf-8",
    )

    payload = _live_chats(repo, 29)

    assert [artifact["id"] for artifact in payload["artifacts"]] == ["trellis_worker_69_result"]
