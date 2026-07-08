from __future__ import annotations

import json
from pathlib import Path

import trellis.viewer_adapter as viewer_adapter
from trellis.viewer_adapter import (
    GitSnapshot,
    RuntimeInfo,
    WorkingTreeSnapshot,
    _awaiting_human_input,
    _bridge_corr_issues,
    _build_nodes_from_snapshot,
    _extract_last_review,
    _feedback_post,
    _live_chats,
    _historical_activity_map,
    _historical_open_blockers,
    _meta_source,
    _resolve_backend_descriptor,
)


def test_feedback_post_stamps_cycle_into_gate_response(monkeypatch, tmp_path) -> None:
    # Defect 1: the viewer must stamp the live protocol cycle into
    # `human_gate_response.json` so the bridge/kernel can reject a stale
    # response. Without the stamp a later gate would auto-consume it.
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    protocol_state_path = runtime_root / "protocol_state.json"
    protocol_state_path.write_text(json.dumps({"cycle": 468}), encoding="utf-8")
    info = RuntimeInfo(
        root=runtime_root,
        protocol_state_path=protocol_state_path,
        metadata_path=runtime_root / "metadata.json",
        bridge_dir=None,
    )
    monkeypatch.setattr(viewer_adapter, "_load_runtime_info", lambda _repo: info)

    result = _feedback_post(tmp_path, action="approve", feedback="")
    assert result["ok"] is True

    response = json.loads((runtime_root / "human_gate_response.json").read_text())
    assert response == {"choice": "approve", "cycle": 468}
    meta = json.loads(
        (runtime_root / "human_gate_response.viewer_meta.json").read_text()
    )
    assert meta == {"choice": "approve", "cycle": 468}


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
    # Per-cycle event log: one file per cycle inside the tracked repo tree.
    event_log_dir = repo / ".trellis-history" / "event-log"
    event_log_dir.mkdir(parents=True)
    (event_log_dir / "cycle-000028.jsonl").write_text(
        json.dumps({"commands": [{"command": "issue_request", "request": {"id": 68, "cycle": 28}}]}) + "\n",
        encoding="utf-8",
    )
    (event_log_dir / "cycle-000029.jsonl").write_text(
        json.dumps({"commands": [{"command": "issue_request", "request": {"id": 69, "cycle": 29}}]}) + "\n",
        encoding="utf-8",
    )

    payload = _live_chats(repo, 29)

    assert [artifact["id"] for artifact in payload["artifacts"]] == ["trellis_worker_69_result"]


def _write_config(repo: Path, *, default_target: str | None) -> None:
    workflow: dict = {"start_phase": "theorem_stating"}
    if default_target is not None:
        workflow["default_target"] = default_target
    (repo / "trellis.config.json").write_text(
        json.dumps({"repo_path": str(repo), "workflow": workflow}),
        encoding="utf-8",
    )


def test_backend_descriptor_resolves_isabelle_from_default_target(tmp_path: Path) -> None:
    isa = tmp_path / "isa"
    isa.mkdir()
    _write_config(isa, default_target="isabelle_hol")
    lean = tmp_path / "lean"
    lean.mkdir()
    _write_config(lean, default_target=None)

    isa_desc = _resolve_backend_descriptor(isa)
    lean_desc = _resolve_backend_descriptor(lean)

    assert isa_desc["backend"] == "isabelle_hol"
    assert isa_desc["nodeExt"] == "thy"
    assert isa_desc["hasBodyMarker"] is False
    # Lean arm = current behaviour verbatim: a `default_target`-absent config
    # resolves to the Lean descriptor with the literal labels rendered today.
    assert lean_desc["backend"] == "lean"
    assert lean_desc["nodeExt"] == "lean"
    assert lean_desc["hasBodyMarker"] is True
    assert lean_desc["labels"]["certViolations"] == "axioms"
    # An entirely missing config still resolves to the Lean default.
    assert _resolve_backend_descriptor(tmp_path / "missing")["backend"] == "lean"


def test_build_nodes_reads_thy_source_for_isabelle_run(tmp_path: Path) -> None:
    # Mirror of the existing `.lean` fixture, but a `.thy`-sourced Isabelle
    # run: the load-bearing fix is that `leanContent` is populated from the
    # `.thy` file (it was empty before — every node then showed "(none)").
    repo = tmp_path / "repo"
    tablet = repo / "Tablet"
    tablet.mkdir(parents=True)
    _write_config(repo, default_target="isabelle_hol")
    (tablet / "Preamble.lean").write_text("", encoding="utf-8")  # placeholder
    (tablet / "Preamble.thy").write_text(
        "theory Tablet_Preamble\n  imports Main\nbegin\n\nend\n", encoding="utf-8"
    )
    isolated_thy = (
        "theory Tablet_Isolated\n  imports Tablet_Preamble\nbegin\n\n"
        'definition Isolated :: "nat set set \\<Rightarrow> nat \\<Rightarrow> bool" where\n'
        '  "Isolated G v = (\\<forall>e\\<in>G. v \\<notin> e)"\n\nend\n'
    )
    (tablet / "Isolated.thy").write_text(isolated_thy, encoding="utf-8")
    (tablet / "Isolated.tex").write_text(
        "\\begin{definition}[Isolated]\\label{def:isolated}\\end{definition}\n",
        encoding="utf-8",
    )

    descriptor = _resolve_backend_descriptor(repo)
    # Kernel tracks Preamble as a committed/closed node; Isolated is an
    # untracked on-disk draft (so it must render open without a sorry-scan).
    protocol_state = {
        "live": {"present_nodes": ["Preamble"], "open_nodes": []},
        "committed": {"present_nodes": ["Preamble"], "open_nodes": []},
    }
    nodes = _build_nodes_from_snapshot(
        snapshot=WorkingTreeSnapshot(repo_path=repo, node_ext=descriptor["nodeExt"]),
        protocol_state=protocol_state,
        bridge_state={},
        descriptor=descriptor,
    )

    assert set(nodes) == {"Preamble", "Isolated"}
    # The load-bearing assertion: `.thy` source flows into `leanContent`.
    assert "Isolated G v" in nodes["Isolated"]["leanContent"]
    assert len(nodes["Isolated"]["leanContent"]) > 0
    # Status is kernel-sourced (sign-off #3), not from a `.thy` sorry-scan.
    assert nodes["Preamble"]["status"] == "closed"  # tracked, not open
    assert nodes["Isolated"]["status"] == "open"     # untracked draft
    assert nodes["Isolated"]["hasSorry"] is False     # no Isabelle sorry-scan


def test_build_nodes_lean_run_unchanged_and_reads_lean_source(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    tablet = repo / "Tablet"
    tablet.mkdir(parents=True)
    _write_config(repo, default_target=None)  # Lean default
    (tablet / "Preamble.lean").write_text("import Mathlib\n", encoding="utf-8")
    closed_lean = (
        "import Tablet.Preamble\n-- [TABLET NODE: Foo]\n"
        "theorem Foo : 1 = 1 := by\n-- BODY\n  rfl\n"
    )
    (tablet / "Foo.lean").write_text(closed_lean, encoding="utf-8")
    open_lean = (
        "import Tablet.Preamble\n-- [TABLET NODE: Bar]\n"
        "theorem Bar : 2 = 2 := by\n-- BODY\n  sorry\n"
    )
    (tablet / "Bar.lean").write_text(open_lean, encoding="utf-8")

    descriptor = _resolve_backend_descriptor(repo)
    assert descriptor["nodeExt"] == "lean"
    protocol_state = {
        "live": {"present_nodes": ["Preamble", "Foo", "Bar"], "open_nodes": ["Bar"]},
        "committed": {"present_nodes": ["Preamble", "Foo", "Bar"], "open_nodes": ["Bar"]},
        "proof_nodes": ["Foo", "Bar"],
    }
    nodes = _build_nodes_from_snapshot(
        snapshot=WorkingTreeSnapshot(repo_path=repo, node_ext=descriptor["nodeExt"]),
        protocol_state=protocol_state,
        bridge_state={},
        descriptor=descriptor,
    )

    assert set(nodes) == {"Preamble", "Foo", "Bar"}
    # Lean source still flows into leanContent and the cheap sorry-scan still
    # drives status/hasSorry (current behaviour verbatim).
    assert "theorem Foo" in nodes["Foo"]["leanContent"]
    assert nodes["Foo"]["status"] == "closed"
    assert nodes["Foo"]["hasSorry"] is False
    assert nodes["Bar"]["status"] == "open"
    assert nodes["Bar"]["hasSorry"] is True


def test_build_nodes_renders_pv_waived_corr_and_substantiveness_solid(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    tablet = repo / "Tablet"
    tablet.mkdir(parents=True)
    _write_config(repo, default_target=None)
    (tablet / "Preamble.lean").write_text("import Mathlib\n", encoding="utf-8")
    (tablet / "Preamble.tex").write_text("", encoding="utf-8")
    for name, declaration in {
        "Model": "def Model : Nat :=\n-- BODY\n  1\n",
        "PinnedTarget": "theorem PinnedTarget : True := by\n-- BODY\n  sorry\n",
        "Assumptions": "axiom Assumptions_axiom : True\n",
        "WorkerDef": "def WorkerDef : Nat :=\n-- BODY\n  2\n",
    }.items():
        (tablet / f"{name}.lean").write_text(
            f"import Tablet.Preamble\n-- [TABLET NODE: {name}]\n{declaration}",
            encoding="utf-8",
        )
        (tablet / f"{name}.tex").write_text(
            f"\\begin{{definition}}[{name}]\\label{{def:{name}}}\\end{{definition}}\n",
            encoding="utf-8",
        )

    protocol_state = {
        "phase": "TheoremStating",
        "live": {
            "present_nodes": ["Preamble", "Model", "PinnedTarget", "Assumptions", "WorkerDef"],
            "open_nodes": ["PinnedTarget"],
        },
        "committed": {
            "present_nodes": ["Preamble", "Model", "PinnedTarget", "Assumptions", "WorkerDef"],
            "open_nodes": ["PinnedTarget"],
        },
        "node_role": {
            "Model": "extraction_model",
            "Assumptions": "under_model_assumptions",
        },
        "challenge_claims": {
            "PinnedTarget": ["PinnedTarget"],
        },
        "substantiveness_status": {},
        "substantiveness_approved_fingerprints": {},
        "corr_status": {},
        "corr_approved_fingerprints": {},
    }

    nodes = _build_nodes_from_snapshot(
        snapshot=WorkingTreeSnapshot(repo_path=repo, node_ext="lean"),
        protocol_state=protocol_state,
        bridge_state={},
    )

    assert nodes["Model"]["verification"]["correspondence"] == "pass"
    assert nodes["Model"]["verification"]["substantiveness"] == "pass"
    assert nodes["PinnedTarget"]["verification"]["correspondence"] == "pass"
    assert nodes["PinnedTarget"]["verification"]["substantiveness"] == "pass"
    assert nodes["Assumptions"]["verification"]["substantiveness"] == "pass"
    assert nodes["Assumptions"]["verification"]["correspondence"] == "?"
    assert nodes["WorkerDef"]["verification"]["correspondence"] == "?"
    assert nodes["WorkerDef"]["verification"]["substantiveness"] == "?"
