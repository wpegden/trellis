from __future__ import annotations

import base64
import hashlib
import json
import re
import shutil
import subprocess
import sys
from contextlib import contextmanager
from pathlib import Path
from types import SimpleNamespace

import pytest

from scripts.pv_goal_binding import (
    GoalBindingError,
    _adaptation_masks,
    build_goal_binding_report,
    report_is_complete,
    validate_goal_binding_report,
)
from scripts.pv_phase0 import (
    BROKER_RESULT_SCHEMA,
    ExistingLaneDispatcher,
    Phase0CheckerBroker,
    PHASE0_PROMPT_CONTRACT_VERSION,
    REVISION_HISTORY_HEADER,
    extract_stage_diagnostic,
    render_revision_history,
    Phase0BridgeError,
    Phase0DraftRejected,
    Phase0KernelRejection,
    _read_agent_json,
    Phase0EnvironmentalFailure,
    Phase0Driver,
    dry_run_snapshot,
    run_phase0,
    seal_audit_result,
    seal_worker_draft,
)
from scripts.pv_source_tree import (
    build_source_partition_manifest,
    canonical_json,
    source_tree_manifest,
)
from trellis.adapters import BurstResult, ProviderConfig
from trellis.agent_wrapper.executor import execute_agent_request
from trellis.agent_wrapper.protocol import AgentLane, SingleAgentRequest
from trellis.config import SandboxConfig
from trellis import checking, sandbox as sandbox_module


ROOT = Path(__file__).resolve().parents[1]


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _draft(request_id: str, receipt: str, cargo: bytes, *, thread: str = "thread-1") -> dict:
    quote = "missing standalone Cargo.toml"
    return {
        "schema": "trellis-phase0-worker-draft/v1",
        "request_id": request_id,
        "worker_thread_id": thread,
        "entries": [
            {
                "file": "Cargo.toml",
                "operation": "add",
                "before_span": {"start": 0, "end": 0},
                "after_span": {"start": 0, "end": len(cargo)},
                "before_sha256": None,
                "after_sha256": _sha(cargo),
                "patch": {
                    "removed_base64": "",
                    "inserted_base64": base64.b64encode(cargo).decode("ascii"),
                },
                "quoted_error": {
                    "tool": "phase0-checker",
                    "receipt_sha256": receipt,
                    "stage_index": 0,
                    "stream": "stderr",
                    "byte_offset": 0,
                    "quote": quote,
                },
                "behavior_preservation_claim": "Adds only a standalone package boundary.",
                "seam_class": "packaging_shim",
                "citation": {
                    "kind": "language_guarantee",
                    "statement": "The library path selects the unchanged source body.",
                },
                "affected_targets": ["goal:target"],
                "meaning_change": False,
            }
        ],
    }


def test_worker_draft_is_strict_and_framework_seals_every_computed_binding(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    candidate = tmp_path / "candidate"
    raw.mkdir()
    candidate.mkdir()
    source = b"pub fn target() {}\n"
    (raw / "lib.rs").write_bytes(source)
    (candidate / "lib.rs").write_bytes(source)
    cargo = b"[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n"
    (candidate / "Cargo.toml").write_bytes(cargo)
    receipt = _sha(b"discovery")
    request = {"request_id": "worker-0", "request_sha256": _sha(b"request")}
    state = {
        "generation": 0,
        "genesis": {
            "unadapted_source_manifest": source_tree_manifest(raw),
            "goal_sha256": _sha(b"goal"),
        },
    }
    response = seal_worker_draft(_draft("worker-0", receipt, cargo), request, state, candidate)
    entry = response["ledger"]["entries"][0]
    assert response["schema"] == "trellis-phase0-worker-response/v1"
    assert entry["id"].startswith("phase0-1-")
    assert entry["discovery_failure_receipt_sha256"] == receipt
    assert entry["ablation_failure_receipt_sha256"] == "0" * 64
    assert response["candidate_manifest"] == source_tree_manifest(candidate)
    assert response["response_sha256"] == _sha_tagged_unsigned(response, "response_sha256")

    malformed = _draft("worker-0", receipt, cargo)
    malformed["entries"][0]["rewrite_pattern"] = "campaign-specific"
    with pytest.raises(Phase0BridgeError, match="unknown fields"):
        seal_worker_draft(malformed, request, state, candidate)


def _sha_tagged_unsigned(value: dict, field: str) -> str:
    from scripts.pv_source_tree import tagged_hash

    unsigned = dict(value)
    unsigned.pop(field)
    return tagged_hash("raw-artifact", canonical_json(unsigned))


def test_goal_binding_report_is_shared_complete_and_self_digested(tmp_path: Path) -> None:
    tree = tmp_path / "crate"
    tree.mkdir()
    (tree / "lib.rs").write_text("pub fn target() {}\n", encoding="utf-8")
    goal = tmp_path / "GOAL.md"
    goal.write_text("### `target`\nResolution: decide\n", encoding="utf-8")
    tablet = tmp_path / "pv_tablet.json"
    tablet.write_text(
        json.dumps(
            {
                "extraction_models": [
                    {
                        "node": "Target",
                        "lean": "/-- [fixture::target]:\n    Source: 'lib.rs' -/\ndef target := 1\n",
                        "provenance": {"source_sha256": _sha((tree / "lib.rs").read_bytes())},
                    }
                ]
            }
        ),
        encoding="utf-8",
    )
    report = build_goal_binding_report(goal, tablet, tree, tree, {"entries": []})
    validate_goal_binding_report(report)
    assert report_is_complete(report)
    assert report["targets"][0]["matches"][0]["adapted_item_path"] == "fixture::target"
    assert report["targets"][0]["origin"] == "target"
    assert report["targets"][0]["goal_binding_outside_target"] is False
    assert report["targets"][0]["goal_binding_in_adaptation"] is False

    sealed = tmp_path / "sealed.json"
    sealed.write_bytes(
        canonical_json(
            {
                "schema": "trellis-phase0-sealed-generation/v1",
                "semantic_bundle": {"goal_binding_report": report},
            }
        )
    )
    output = tmp_path / "binding.json"
    command = [
        sys.executable,
        str(ROOT / "scripts/pv_goal_binding.py"),
        "--goal",
        str(goal),
        "--pv-tablet",
        str(tablet),
        "--adapted-tree",
        str(tree),
        "--unadapted-tree",
        str(tree),
        "--out",
        str(output),
        "--require-complete",
        "--expected-sealed-generation",
        str(sealed),
    ]
    assert subprocess.run(command, cwd=ROOT, check=False).returncode == 0
    assert output.read_bytes() == canonical_json(report)

    changed = json.loads(sealed.read_bytes())
    changed["semantic_bundle"]["goal_binding_report"]["targets"][0]["status"] = "missing"
    sealed.write_bytes(canonical_json(changed))
    output.unlink()
    refused = subprocess.run(command, cwd=ROOT, text=True, capture_output=True, check=False)
    assert refused.returncode != 0
    assert "differs byte-for-byte from the sealed report" in refused.stderr
    assert not output.exists()


def test_goal_binding_flags_added_shim_but_not_unmodified_upstream_target(
    tmp_path: Path,
) -> None:
    raw = tmp_path / "raw"
    adapted = tmp_path / "adapted"
    raw.mkdir()
    adapted.mkdir()
    upstream = b"pub fn upstream() {}\n"
    shim = b"pub fn shim() {}\n"
    (raw / "lib.rs").write_bytes(upstream)
    (adapted / "lib.rs").write_bytes(upstream)
    (adapted / "shim.rs").write_bytes(shim)
    goal = tmp_path / "GOAL.md"
    goal.write_text(
        "### `shim`\nResolution: decide\n### `upstream`\nResolution: decide\n",
        encoding="utf-8",
    )
    tablet = tmp_path / "pv_tablet.json"
    tablet.write_text(
        json.dumps(
            {
                "extraction_models": [
                    {
                        "node": "Shim",
                        "lean": "/-- [fixture::shim]: Source: 'shim.rs', lines 1:0-1:16 -/\ndef shim := 1\n",
                        "provenance": {"source_sha256": _sha(shim)},
                    },
                    {
                        "node": "Upstream",
                        "lean": "/-- [fixture::upstream]: Source: 'lib.rs', lines 1:0-1:20 -/\ndef upstream := 1\n",
                        "provenance": {"source_sha256": _sha(upstream)},
                    },
                ]
            }
        ),
        encoding="utf-8",
    )
    ledger = {
        "entries": [
            {
                "file": "shim.rs",
                "operation": "add",
                "before_span": {"start": 0, "end": 0},
                "patch": {
                    "removed_base64": "",
                    "inserted_base64": base64.b64encode(shim).decode("ascii"),
                },
            }
        ]
    }
    report = build_goal_binding_report(goal, tablet, adapted, raw, ledger)
    flags = {
        target["name"]: (
            target["origin"],
            target["goal_binding_outside_target"],
            target["goal_binding_in_adaptation"],
        )
        for target in report["targets"]
    }
    assert flags == {
        "shim": ("adaptation", True, True),
        "upstream": ("target", False, False),
    }


def test_goal_binding_replays_several_entries_in_one_file(tmp_path: Path) -> None:
    """Every span is measured in the uploaded file, so replaying them in place
    has to rebase each one over the earlier spans of the same file, exactly as
    the kernel does. Without that, no file may carry more than one entry."""
    raw = tmp_path / "raw"
    adapted = tmp_path / "adapted"
    raw.mkdir()
    adapted.mkdir()
    upstream = b"alpha beta gamma\n"
    (raw / "lib.rs").write_bytes(upstream)
    # Two edits whose inserted text is longer than what it replaces, so the
    # second span only lands correctly once the first one's growth is applied.
    (adapted / "lib.rs").write_bytes(b"ALPHA-ONE beta GAMMA-TWO\n")
    ledger = {
        "entries": [
            {
                "file": "lib.rs",
                "operation": "modify",
                "before_span": {"start": 0, "end": 5},
                "patch": {
                    "removed_base64": base64.b64encode(b"alpha").decode("ascii"),
                    "inserted_base64": base64.b64encode(b"ALPHA-ONE").decode("ascii"),
                },
            },
            {
                "file": "lib.rs",
                "operation": "modify",
                "before_span": {"start": 11, "end": 16},
                "patch": {
                    "removed_base64": base64.b64encode(b"gamma").decode("ascii"),
                    "inserted_base64": base64.b64encode(b"GAMMA-TWO").decode("ascii"),
                },
            },
        ]
    }
    masks = _adaptation_masks(raw, adapted, ledger, build_source_partition_manifest(source_tree_manifest(raw), {"lib.rs"}))
    mask = masks["lib.rs"]
    assert len(mask) == len(b"ALPHA-ONE beta GAMMA-TWO\n")
    assert mask[:9] == ["inserted"] * 9
    assert mask[9:15] == ["unadapted_target"] * 6
    assert mask[15:24] == ["inserted"] * 9

    overlapping = {
        "entries": [
            ledger["entries"][0],
            {
                "file": "lib.rs",
                "operation": "modify",
                "before_span": {"start": 3, "end": 9},
                "patch": {
                    "removed_base64": base64.b64encode(b"ha bet").decode("ascii"),
                    "inserted_base64": base64.b64encode(b"x").decode("ascii"),
                },
            },
        ]
    }
    with pytest.raises(GoalBindingError, match="overlap"):
        _adaptation_masks(raw, adapted, overlapping, build_source_partition_manifest(source_tree_manifest(raw), {"lib.rs"}))


def test_goal_binding_discloses_supporting_origin_and_rejects_inconsistent_flag(
    tmp_path: Path,
) -> None:
    raw = tmp_path / "raw"
    raw.mkdir()
    target = b"pub fn target() {}\n"
    supporting = b"pub fn helper() {}\n"
    (raw / "lib.rs").write_bytes(target)
    (raw / "traits.rs").write_bytes(supporting)
    goal = tmp_path / "GOAL.md"
    goal.write_text("### `helper`\nResolution: decide\n", encoding="utf-8")
    tablet = tmp_path / "pv_tablet.json"
    tablet.write_text(
        json.dumps(
            {
                "extraction_models": [
                    {
                        "node": "Helper",
                        "lean": "/-- [fixture::helper]: Source: 'traits.rs' -/\ndef helper := 1\n",
                        "provenance": {"source_sha256": _sha(supporting)},
                    }
                ]
            }
        ),
        encoding="utf-8",
    )
    manifest = source_tree_manifest(raw)
    partition = build_source_partition_manifest(manifest, {"lib.rs"})
    report = build_goal_binding_report(
        goal, tablet, raw, raw, {"entries": []}, partition
    )
    binding = report["targets"][0]
    assert binding["origin"] == "supporting"
    assert binding["goal_binding_outside_target"] is True
    assert binding["goal_binding_in_adaptation"] is False
    validate_goal_binding_report(report)

    malformed = json.loads(json.dumps(report))
    malformed["targets"][0]["goal_binding_outside_target"] = False
    malformed["report_sha256"] = _sha_tagged_unsigned(
        malformed, "report_sha256"
    )
    with pytest.raises(Exception, match="target is malformed"):
        validate_goal_binding_report(malformed)


def _opacity_tablet(tmp_path: Path, tree: Path) -> Path:
    source = b"pub fn target() {}\npub fn plain() {}\npub fn gone() {}\n"
    (tree / "lib.rs").write_bytes(source)
    tablet = tmp_path / "pv_tablet.json"
    tablet.write_text(
        json.dumps(
            {
                "extraction_models": [
                    {
                        "node": "Target",
                        "lean": "/-- [fixture::target]: Source: 'lib.rs' -/\n"
                        "def target := fixture.helper 1\n",
                        "provenance": {"source_sha256": _sha(source)},
                    },
                    {
                        "node": "Plain",
                        "lean": "/-- [fixture::plain]: Source: 'lib.rs' -/\n"
                        "def plain := 1\n",
                        "provenance": {"source_sha256": _sha(source)},
                    },
                    {
                        "node": "Gone",
                        "lean": "/-- [fixture::gone]: Source: 'lib.rs' -/\n"
                        "axiom fixture.gone : Nat\n",
                        "provenance": {"source_sha256": _sha(source)},
                    },
                ]
            }
        ),
        encoding="utf-8",
    )
    return tablet


_OPACITY_GOAL = (
    "### `gone`\nResolution: decide\n"
    "### `plain`\nResolution: decide\n"
    "### `target`\nResolution: decide\n"
)

_OPACITY_SCAN = {
    "status": "read",
    "read_from": "generated.lean",
    "detail": "",
    "declarations": [
        {"name": "fixture.gone", "kind": "axiom", "item_path": "fixture::gone"},
        {"name": "fixture.helper", "kind": "axiom", "item_path": "fixture::helper"},
    ],
}


def test_goal_binding_reports_bodiless_model_declarations_per_target(
    tmp_path: Path,
) -> None:
    tree = tmp_path / "crate"
    tree.mkdir()
    tablet = _opacity_tablet(tmp_path, tree)
    goal = tmp_path / "GOAL.md"
    goal.write_text(_OPACITY_GOAL, encoding="utf-8")

    report = build_goal_binding_report(
        goal, tablet, tree, tree, {"entries": []}, None, _OPACITY_SCAN
    )
    validate_goal_binding_report(report)
    assert report["model_bodiless_status"] == "read"
    assert report["model_bodiless_scope"] == "direct_calls_only"
    assert report["model_bodiless_declarations"] == _OPACITY_SCAN["declarations"]
    flags = {
        target["name"]: (
            target["model_declaration_bodiless"],
            target["model_bodiless_direct_calls"],
            target["model_computation_suppressed"],
        )
        for target in report["targets"]
    }
    assert flags == {
        # Its own extracted declaration has no body.
        "gone": (True, [], True),
        "plain": (False, [], False),
        # Its body is modelled, but a computation it calls is not.
        "target": (False, ["fixture.helper"], True),
    }

    malformed = json.loads(json.dumps(report))
    malformed["targets"][2]["model_computation_suppressed"] = False
    malformed["report_sha256"] = _sha_tagged_unsigned(malformed, "report_sha256")
    with pytest.raises(Exception, match="opacity facts are inconsistent"):
        validate_goal_binding_report(malformed)


def test_goal_binding_without_a_model_scan_says_unread_not_clean(
    tmp_path: Path,
) -> None:
    tree = tmp_path / "crate"
    tree.mkdir()
    tablet = _opacity_tablet(tmp_path, tree)
    goal = tmp_path / "GOAL.md"
    goal.write_text(_OPACITY_GOAL, encoding="utf-8")

    report = build_goal_binding_report(goal, tablet, tree, tree, {"entries": []})
    validate_goal_binding_report(report)
    assert report["model_bodiless_status"] == "not_extracted"
    assert report["model_bodiless_declarations"] == []
    assert not any(
        target["model_computation_suppressed"] for target in report["targets"]
    )


def test_goal_binding_cli_scans_the_model_directory_it_is_given(
    tmp_path: Path,
) -> None:
    tree = tmp_path / "crate"
    tree.mkdir()
    tablet = _opacity_tablet(tmp_path, tree)
    goal = tmp_path / "GOAL.md"
    goal.write_text(_OPACITY_GOAL, encoding="utf-8")
    model = tmp_path / "extracted"
    model.mkdir()
    (model / "generated.lean").write_text(
        "/-- [fixture::gone]: -/\naxiom fixture.gone : Nat\n"
        "/-- [fixture::helper]: -/\naxiom fixture.helper : Nat\n",
        encoding="utf-8",
    )
    output = tmp_path / "binding.json"
    completed = subprocess.run(
        [
            sys.executable,
            str(ROOT / "scripts/pv_goal_binding.py"),
            "--goal", str(goal),
            "--pv-tablet", str(tablet),
            "--adapted-tree", str(tree),
            "--unadapted-tree", str(tree),
            "--model-root", str(model),
            "--out", str(output),
        ],
        cwd=ROOT,
        check=False,
    )
    assert completed.returncode == 0
    report = json.loads(output.read_bytes())
    assert report["model_bodiless_status"] == "read"
    assert [row["name"] for row in report["model_bodiless_declarations"]] == [
        "fixture.gone",
        "fixture.helper",
    ]


def test_only_operator_amended_phase0_role_fragments_are_registered() -> None:
    root = Path("trellis/prompt_fragments/pv/source_adaptation")
    files = sorted(path.relative_to(root).as_posix() for path in root.rglob("*.md"))
    assert files == [
        "verifier/correspondence/05_role.md",
        "verifier/deviation/05_role.md",
        "worker/05_role.md",
    ]
    for relative in files:
        text = (root / relative).read_text(encoding="utf-8").strip()
        assert 15 <= len(text.split()) <= 40
        assert "\n\n" not in text
        assert "for example" not in text.lower()
        assert "instead of" not in text.lower()


def test_phase0_sandbox_write_surface_and_existing_worker_dispatch(
    tmp_path: Path, monkeypatch
) -> None:
    attempt = tmp_path / "attempt"
    attempt.mkdir()
    paths = sandbox_module._repo_writable_paths(  # noqa: SLF001 - contract-level test
        attempt, role="source_adaptation_worker"
    )
    assert paths == [
        attempt / "candidate",
        attempt / "agent-output",
        attempt / ".trellis" / "chats",
    ]
    assert sandbox_module._repo_writable_paths(  # noqa: SLF001
        attempt, role="source_adaptation_reviewer"
    ) == [attempt / "agent-output", attempt / ".trellis" / "chats"]
    # The checker client next to the transcript store stays outside the
    # writable surface: only `.trellis/chats` is bound writable.
    assert attempt / ".trellis" not in paths
    assert attempt / ".trellis" / "scripts" not in paths
    checker_client = checking.write_phase0_candidate_script(attempt)
    assert checker_client == attempt / ".trellis" / "scripts" / "check.py"
    assert checker_client.stat().st_mode & 0o222 == 0
    client_text = checker_client.read_text(encoding="utf-8")
    assert "phase0-candidate" in client_text
    assert "import trellis" not in client_text.lower()
    refused = subprocess.run(
        [sys.executable, str(checker_client), "phase0-candidate", "/agent/candidate"],
        capture_output=True,
        check=False,
    )
    assert refused.returncode == 2
    with pytest.raises(ValueError, match="explicit burst_home"):
        sandbox_module.wrap_command(
            ["true"], sandbox=SandboxConfig(), work_dir=attempt,
            role="source_adaptation_worker",
        )

    calls: list[dict] = []

    def worker_runner(*_args, **kwargs):
        calls.append(kwargs)
        return BurstResult(ok=True, exit_code=0, captured_output="", duration_seconds=0)

    def reviewer_runner(*_args, **_kwargs):
        raise AssertionError("source-adaptation worker was routed to reviewer identity")

    home = attempt / "burst-homes" / "worker"
    home.mkdir(parents=True)
    response = execute_agent_request(
        SingleAgentRequest(
            request_id="phase0-worker",
            cycle=0,
            kind="phase0",
            burst_role="source_adaptation_worker",
            provider=ProviderConfig("stub"),
            prompt="contract",
            work_dir=attempt,
            state_dir=attempt / "agent-output",
            session_name="phase0-worker",
            session_scope="source_adaptation:worker:binding",
            lane=AgentLane(kind="worker"),
            timeout_seconds=1,
            burst_home=home,
            fresh=False,
        ),
        worker_runner=worker_runner,
        reviewer_runner=reviewer_runner,
    )
    assert response.ok
    assert calls[0]["sandbox_role"] == "source_adaptation_worker"
    assert calls[0]["session_scope"] == "source_adaptation:worker:binding"
    assert calls[0]["fresh"] is False

    dispatcher = ExistingLaneDispatcher(
        attempt,
        {"bindings": {"worker": {"binding_sha256": _sha(b"worker")}}},
    )
    monkeypatch.setattr(dispatcher, "_dispatch", lambda **kwargs: kwargs)
    dispatched = dispatcher.worker(
        {"prompt_fragments": ["worker.md"]},
        {"TRELLIS_PHASE0_CHECKER_SOCKET": "/authority/checker.sock"},
    )
    assert dispatched["environment"] == {
        "TRELLIS_SANDBOX_EXTRA_RO_PATHS": "",
        "TRELLIS_PHASE0_CHECKER_SOCKET": "/authority/checker.sock",
    }


def test_runtime_owned_broker_endpoint_accepts_no_paths(monkeypatch, capsys) -> None:
    sent: list[bytes] = []

    class FakeClient:
        def __init__(self) -> None:
            self.response = canonical_json(
                {
                    "schema": BROKER_RESULT_SCHEMA,
                    "status": "failed",
                    "first_failing_stage": "standalone_package",
                }
            )

        def __enter__(self):
            return self

        def __exit__(self, *_args):
            return None

        def settimeout(self, _seconds):
            return None

        def connect(self, address):
            assert address == "/run/trellis-phase0/checker.sock"

        def sendall(self, payload):
            sent.append(payload)

        def recv(self, _size):
            response, self.response = self.response, b""
            return response

    monkeypatch.setattr(checking.socket, "socket", lambda *_args: FakeClient())
    monkeypatch.setenv("TRELLIS_PHASE0_CHECKER_SOCKET", "/run/trellis-phase0/checker.sock")
    monkeypatch.setenv("TRELLIS_PHASE0_CHECKER_TOKEN", "authority-token")
    assert checking._phase0_candidate_main([]) == 0  # noqa: SLF001
    assert checking._phase0_candidate_main(["/agent/tool", "/agent/candidate"]) == 2  # noqa: SLF001
    lines = capsys.readouterr().out.splitlines()
    result = json.loads(lines[0])
    assert result["schema"] == BROKER_RESULT_SCHEMA
    assert result["status"] == "failed"
    assert json.loads(sent[0]) == {
        "schema": "trellis-phase0-broker-call/v1",
        "operation": "candidate",
        "token": "authority-token",
    }
    assert "path" not in sent[0].decode("utf-8")


def test_audit_draft_is_bound_to_exact_request_and_lane() -> None:
    request = {
        "request_id": "audit-1",
        "request_sha256": _sha(b"request"),
        "scenario": "seam_repair",
        "generation": 3,
        "genesis_sha256": _sha(b"genesis"),
        "semantic_bundle_sha256": _sha(b"bundle"),
        "ledger_sha256": _sha(b"ledger"),
        "lane": {"lane_id": "seam", "binding_sha256": _sha(b"lane")},
    }
    draft = {
        "schema": "trellis-phase0-audit-draft/v1",
        "decision": "pass",
        "entry_verdicts": [],
        "whole_tree_verdict": None,
        "goal_binding_verdicts": [],
        "findings": [],
    }
    result = seal_audit_result(draft, request)
    assert result["request_sha256"] == request["request_sha256"]
    assert result["lane_binding_sha256"] == request["lane"]["binding_sha256"]
    assert result["result_sha256"] == _sha_tagged_unsigned(result, "result_sha256")


def test_bridge_dry_run_exposes_only_candidate_and_delivery_as_worker_writes(tmp_path: Path) -> None:
    snapshot = dry_run_snapshot(tmp_path / "attempt", tmp_path / "authority")
    assert snapshot["worker"]["writable"] == [
        str((tmp_path / "attempt" / "candidate").resolve()),
        str((tmp_path / "attempt" / "agent-output").resolve()),
        str((tmp_path / "attempt" / ".trellis" / "chats").resolve()),
    ]
    assert snapshot["reviewer"]["writable"] == [
        str((tmp_path / "attempt" / "agent-output").resolve()),
        str((tmp_path / "attempt" / ".trellis" / "chats").resolve()),
    ]
    assert snapshot["worker"]["direct_checker_tools"] == []
    assert snapshot["reviewer"]["fresh"] is True
    assert snapshot["authority"]["agent_writable"] is False


@pytest.mark.parametrize("same_model", [False, True])
def test_production_entry_records_explicit_lane_models_and_relays_independence_refusal(
    tmp_path: Path, monkeypatch, same_model: bool
) -> None:
    raw = tmp_path / "raw"
    raw.mkdir()
    (raw / "module.rs").write_bytes(b"pub fn target() {}\n")
    goal = tmp_path / "GOAL.md"
    goal.write_text("### `target`\nResolution: decide\n", encoding="utf-8")
    archive = tmp_path / "source.tar.gz"
    archive.write_bytes(b"frozen upload bytes")
    partition_path = tmp_path / "source-partition-manifest.json"
    raw_manifest = source_tree_manifest(raw)
    partition = build_source_partition_manifest(raw_manifest, {"module.rs"})
    partition_path.write_bytes(canonical_json(partition))
    attempt = tmp_path / "attempt"
    attempt.mkdir()
    authority = tmp_path / "authority"
    tools = tmp_path / "tools"
    tools.mkdir()
    opam = tmp_path / "opam"
    opam.mkdir()
    (opam / "pin").write_bytes(b"opam")

    def executable(name: str, body: str) -> Path:
        path = tools / name
        path.write_text(body, encoding="utf-8")
        path.chmod(0o700)
        return path

    cargo = executable("cargo", "#!/bin/sh\necho cargo-fixture\n")
    rustc = executable("rustc", "#!/bin/sh\necho rustc-fixture\n")
    kernel = executable(
        "kernel",
        "#!/usr/bin/python3\nimport json,pathlib,sys\n"
        "genesis=json.load(open(sys.argv[2], encoding='utf-8'))\n"
        "worker=genesis['bindings']['worker']\n"
        "lanes=genesis['bindings']['seam_repair']+genesis['bindings']['source_correspondence']\n"
        "if not genesis['policy']['allow_same_model_lanes'] and any(\n"
        "    lane['provider']==worker['provider'] and lane['model']==worker['model'] for lane in lanes):\n"
        "    sys.stderr.write('Phase-0 audit lane shares the worker provider/model without phase0.allow_same_model_lanes')\n"
        "    raise SystemExit(19)\n"
        "pathlib.Path(sys.argv[5]).write_text('{}', encoding='utf-8')\n",
    )
    controller = executable("controller", "#!/bin/sh\nexit 0\n")
    lane_config = tmp_path / "lanes.json"
    lane_config.write_text(
        json.dumps(
            {
                "worker": {"provider": "stub", "model": "worker", "effort": "fixed"},
                "reviewer": {"provider": "stub", "model": "seam", "effort": "fixed"},
                "verification": {
                    "correspondence_agents": [
                        {"provider": "stub", "model": "corr", "effort": "fixed"}
                    ]
                },
                "phase0": {
                    "allow_same_model_lanes": False,
                    "adapting_worker": {
                        "provider": "stub", "model": "phase-worker", "effort": "fixed"
                    },
                    "seam_repair_auditor": {
                        "provider": "stub",
                        "model": "phase-worker" if same_model else "phase-seam",
                        "effort": "high",
                    },
                    "correspondence_agents": [
                        {
                            "provider": "stub", "model": "phase-corr", "effort": "xhigh"
                        }
                    ],
                },
            }
        ),
        encoding="utf-8",
    )
    monkeypatch.setattr(
        "scripts.pv_phase0.shutil.which",
        lambda name: str(cargo if name == "cargo" else rustc if name == "rustc" else ""),
    )

    def checkout(path: Path) -> dict:
        return {
            "executable": {"path": str(path), "sha256": _sha(path.read_bytes())},
            "source": {
                "path": str(opam),
                "source_tree_sha256": source_tree_manifest(opam)["source_tree_sha256"],
            },
            "revision": "1" * 40,
        }

    monkeypatch.setattr("scripts.pv_phase0._checkout_tool", checkout)
    monkeypatch.setattr("scripts.pv_phase0._charon_toolchain_root", lambda _path: opam)
    monkeypatch.setattr(
        "scripts.pv_phase0._resolve_opam_switch_environment",
        lambda _root, _switch: (
            opam,
            {
                "OPAM_SWITCH_PREFIX": str(opam),
                "OPAMSWITCH": "fixture",
                "CAML_LD_LIBRARY_PATH": str(opam),
                "OCAML_TOPLEVEL_PATH": str(opam),
                "OCAMLTOP_INCLUDE_PATH": str(opam),
            },
        ),
    )

    class FakeKernel:
        def __init__(self, _executable, _state, _context, root):
            self.authority = root

    class FakeBroker:
        def __init__(self, **_kwargs):
            pass

    class FakeDispatcher:
        def __init__(self, _attempt, _genesis):
            self.worker = lambda *_args: {}
            self.reviewer = lambda *_args: {}

    class FakeDriver:
        def __init__(self, *, kernel, **_kwargs):
            self.root = kernel.authority

        def run(self):
            (self.root / "sealed-generation.json").write_bytes(b"{}")
            return {"stage": "sealed"}

    monkeypatch.setattr("scripts.pv_phase0.KernelPhase0", FakeKernel)
    monkeypatch.setattr("scripts.pv_phase0.Phase0CheckerBroker", FakeBroker)
    monkeypatch.setattr("scripts.pv_phase0.ExistingLaneDispatcher", FakeDispatcher)
    monkeypatch.setattr("scripts.pv_phase0.Phase0Driver", FakeDriver)
    args = SimpleNamespace(
        unadapted_tree=raw,
        goal=goal,
        target_archive=archive,
        supporting_archive=None,
        partition_manifest=partition_path,
        attempt_root=attempt,
        authority_root=authority,
        kernel_cli=kernel,
        controller=controller,
        filespec_checker=kernel,
        prescribed_region_checker=kernel,
        lane_config=lane_config,
        charon_bin=cargo,
        aeneas_bin=rustc,
        opam_root=opam,
        opam_switch="fixture",
        target_triple="fixture-target",
        extraction_profile="dev",
    )
    if same_model:
        with pytest.raises(
            Phase0BridgeError,
            match="shares the worker provider/model without phase0.allow_same_model_lanes",
        ):
            run_phase0(args)
        refused_genesis = json.loads((authority / "genesis.json").read_bytes())
        assert refused_genesis["bindings"]["worker"]["model"] == "phase-worker"
        assert refused_genesis["bindings"]["seam_repair"][0]["model"] == "phase-worker"
        assert refused_genesis["policy"]["allow_same_model_lanes"] is False
        return
    result = run_phase0(args)
    genesis = json.loads((authority / "genesis.json").read_bytes())
    assert result["status"] == "sealed"
    assert genesis["target_upload_sha256"] == _sha(archive.read_bytes())
    assert genesis["supporting_upload_sha256"] is None
    assert genesis["unadapted_source_manifest"] == raw_manifest
    assert genesis["source_partition_manifest"] == partition
    assert genesis["bindings"]["worker"]["provider"] == "stub"
    assert genesis["bindings"]["worker"]["model"] == "phase-worker"
    assert genesis["bindings"]["worker"]["effort"] == "fixed"
    assert genesis["bindings"]["seam_repair"][0]["model"] == "phase-seam"
    assert genesis["bindings"]["seam_repair"][0]["effort"] == "high"
    assert genesis["bindings"]["source_correspondence"][0]["model"] == "phase-corr"
    assert genesis["bindings"]["source_correspondence"][0]["effort"] == "xhigh"
    assert genesis["policy"]["allow_same_model_lanes"] is False
    assert genesis["toolchain"]["opam_switch_prefix"]["path"] == str(opam)
    assert genesis["toolchain"]["opam_switch_environment"]["OPAMSWITCH"] == "fixture"
    assert genesis["components"]["filespec_checker"] == {
        "path": str(kernel),
        "sha256": _sha(kernel.read_bytes()),
    }
    assert genesis["components"]["prescribed_region_checker"] == {
        "path": str(kernel),
        "sha256": _sha(kernel.read_bytes()),
    }
    assert (attempt / "candidate/module.rs").read_bytes() == (raw / "module.rs").read_bytes()
    assert not (attempt / "candidate/Cargo.toml").exists()


def test_stub_worker_adds_package_boundary_after_structured_failure_then_checks_clean(
    tmp_path: Path,
) -> None:
    attempt = tmp_path / "attempt"
    raw = tmp_path / "raw"
    candidate = attempt / "candidate"
    authority = tmp_path / "authority"
    for path in (raw, candidate, authority):
        path.mkdir(parents=True)
    source = b"pub fn target() {}\n"
    (raw / "lib.rs").write_bytes(source)
    (candidate / "lib.rs").write_bytes(source)
    goal = tmp_path / "GOAL.md"
    goal.write_text("### `target`\nResolution: decide\n", encoding="utf-8")
    raw_manifest = source_tree_manifest(raw)
    initial_ledger = {
        "schema": "trellis-phase0-adaptation-ledger/v1",
        "generation": 0,
        "unadapted_tree_sha256": raw_manifest["source_tree_sha256"],
        "adapted_tree_sha256": raw_manifest["source_tree_sha256"],
        "goal_sha256": _sha(goal.read_bytes()),
        "entries": [],
        "ledger_entries_root": _sha(b"initial-ledger-root"),
        "ledger_sha256": _sha(b"initial-ledger"),
    }
    state_path = authority / "state.json"
    context_path = authority / "context.json"
    state = {
        "generation": 0,
        "state_sha256": _sha(b"state-0"),
        "genesis": {
            "genesis_sha256": _sha(b"genesis"),
            "goal_sha256": _sha(goal.read_bytes()),
            "unadapted_source_manifest": raw_manifest,
            "source_partition_manifest": build_source_partition_manifest(
                raw_manifest, {"lib.rs"}
            ),
        },
        "ledger": initial_ledger,
    }
    state_path.write_bytes(canonical_json(state))
    context_path.write_bytes(
        canonical_json(
            {
                "candidate_tree_path": str(candidate.resolve()),
                "unadapted_tree_path": str(raw.resolve()),
                "ledger_path": str((authority / "ledger.json").resolve()),
                "receipt_store_path": str((authority / "receipts").resolve()),
                "goal_binding_report_path": str((authority / "goal-binding.json").resolve()),
            }
        )
    )

    class StubKernel:
        def __init__(self) -> None:
            self.authority = authority.resolve()
            self.state = state_path.resolve()
            self.context = context_path.resolve()
            self.stage = "check"
            self.failure: dict | None = None

        def _state(self) -> dict:
            return json.loads(self.state.read_bytes())

        def _store(self, value: dict) -> None:
            self.state.write_bytes(canonical_json(value))

        def check(self) -> dict:
            return {"stage": self.stage}

        def next(self) -> dict:
            assert self.stage == "worker" and self.failure is not None
            request = {
                "request_id": "worker-after-standalone-failure",
                "request_sha256": _sha(b"worker-request"),
                "checker_failure_receipt_sha256": self.failure["receipt_sha256"],
                "checker_failure_receipt": self.failure,
            }
            return {"kind": "worker", "request": request}

        def apply(self, event: dict) -> dict:
            current = self._state()
            if event["kind"] == "checker_failure":
                self.failure = event["receipt"]
                assert self.failure["parsed_stdout"]["first_failing_stage"] == "standalone_package"
                self.stage = "worker"
            elif event["kind"] == "worker_revision":
                assert (candidate / "Cargo.toml").is_file()
                current["generation"] = event["response"]["ledger"]["generation"]
                current["ledger"] = event["response"]["ledger"]
                current["state_sha256"] = _sha(b"state-1")
                self._store(current)
                self.stage = "check"
            elif event["kind"] == "check_complete":
                assert event["candidate_success_receipt"]["parsed_stdout"]["status"] == "passed"
                assert event["final_success_receipt"]["parsed_stdout"]["status"] == "passed"
                assert len(event["ablation_receipts"]) == 1
                assert report_is_complete(event["goal_binding_report"])
                self.stage = "seam_audit"
            else:
                raise AssertionError(f"unexpected stub event {event['kind']}")
            return self.check()

    class StubBroker:
        def __init__(self) -> None:
            self.receipts = (authority / "receipts").resolve()
            self.receipts.mkdir()
            self.runs = (authority / "runs").resolve()
            self.runs.mkdir()
            self.template = {"goal_path": str(goal.resolve())}
            self.candidate_checks = 0

        @staticmethod
        def _result(kind: str, status: str, stage: str | None = None) -> dict:
            output = {
                "status": status,
                "check_id": kind,
                "first_failing_stage": stage,
                "stages": [] if stage is None else [{"name": stage, "status": "failed"}],
            }
            receipt = {
                "receipt_sha256": _sha(f"receipt:{kind}".encode()),
                "parsed_stdout": output,
            }
            return {"output": output, "receipt": receipt}

        def check(self, kind="candidate", *, candidate=None, reverted_entry_id=None):
            if kind == "candidate":
                self.candidate_checks += 1
                if self.candidate_checks == 1:
                    return self._result(kind, "failed", "standalone_package")
                assert (candidate_path / "Cargo.toml").is_file()
                return self._result("candidate-clean", "passed")
            if kind == "ratification_replay":
                result = self._result("ratification-clean", "passed")
                tablet = (
                    self.runs / "ratification-clean" / "extraction-writable" / "extracted"
                    / "pv_tablet.json"
                )
                tablet.parent.mkdir(parents=True)
                tablet.write_text(
                    json.dumps(
                        {
                            "extraction_models": [
                                {
                                    "node": "Target",
                                    "lean": "/-- [fixture::target]: Source: 'lib.rs' -/\ndef target := 1\n",
                                    "provenance": {"source_sha256": _sha(source)},
                                }
                            ]
                        }
                    ),
                    encoding="utf-8",
                )
                return result
            assert kind == "entry_ablation" and reverted_entry_id
            assert candidate is not None and not (candidate / "Cargo.toml").exists()
            return self._result("ablation", "failed", "standalone_package")

        @contextmanager
        def serve_worker(self, _socket_path):
            yield {
                "TRELLIS_PHASE0_CHECKER_SOCKET": "/authority/phase0.sock",
                "TRELLIS_PHASE0_CHECKER_TOKEN": "authority-token",
            }

        @staticmethod
        def cleanup(_result):
            return None

    candidate_path = candidate
    cargo = b"[package]\nname = \"adapted\"\nversion = \"0.0.0\"\n"
    worker_calls: list[str] = []

    def worker(request: dict, environment: dict[str, str]) -> dict:
        assert request["checker_failure_receipt"]["parsed_stdout"]["first_failing_stage"] == "standalone_package"
        assert set(environment) == {
            "TRELLIS_PHASE0_CHECKER_SOCKET", "TRELLIS_PHASE0_CHECKER_TOKEN"
        }
        worker_calls.append(request["request_id"])
        (candidate / "Cargo.toml").write_bytes(cargo)
        return _draft(request["request_id"], request["checker_failure_receipt_sha256"], cargo)

    driver = Phase0Driver(
        kernel=StubKernel(), broker=StubBroker(), attempt_root=attempt,
        worker=worker, reviewer=lambda _request: pytest.fail("audit was not reached"),
    )
    assert driver.step()["stage"] == "worker"
    assert driver.step()["stage"] == "check"
    assert driver.step()["stage"] == "seam_audit"
    assert worker_calls == ["worker-after-standalone-failure"]


class _JournalBroker:
    def __init__(self, authority: Path, goal: Path) -> None:
        self.receipts = (authority / "receipts").resolve()
        self.receipts.mkdir(parents=True)
        self.template = {"goal_path": str(goal.resolve())}

    @contextmanager
    def serve_worker(self, _socket_path):
        yield {}


def _driver_fixture(tmp_path: Path, worker):
    attempt = tmp_path / "attempt"
    candidate = attempt / "candidate"
    candidate.mkdir(parents=True)
    (candidate / "lib.rs").write_bytes(b"original\n")
    authority = tmp_path / "authority"
    authority.mkdir()
    goal = tmp_path / "GOAL.md"
    goal.write_text("### `target`\nResolution: decide\n", encoding="utf-8")
    state_path = authority / "state.json"
    state_path.write_bytes(canonical_json({"generation": 0, "state_sha256": _sha(b"state-0")}))
    context_path = authority / "context.json"
    context_path.write_bytes(
        canonical_json(
            {
                "candidate_tree_path": str(candidate.resolve()),
                "unadapted_tree_path": str((tmp_path / "raw").resolve()),
                "ledger_path": str((authority / "ledger.json").resolve()),
                "receipt_store_path": str((authority / "receipts").resolve()),
                "goal_binding_report_path": str((authority / "goal-binding.json").resolve()),
            }
        )
    )
    kernel = SimpleNamespace(
        authority=authority.resolve(),
        state=state_path.resolve(),
        context=context_path.resolve(),
    )
    broker = _JournalBroker(authority, goal)
    return Phase0Driver(
        kernel=kernel,
        broker=broker,
        attempt_root=attempt,
        worker=worker,
        reviewer=lambda _request: {},
    ), candidate, authority, state_path


def test_worker_malformed_response_rolls_back_exact_candidate(tmp_path: Path) -> None:
    candidate_holder: list[Path] = []

    def malformed(_request, _environment):
        (candidate_holder[0] / "lib.rs").write_bytes(b"unaccounted\n")
        return {"malformed": True}

    driver, candidate, authority, _state = _driver_fixture(tmp_path, malformed)
    candidate_holder.append(candidate)
    with pytest.raises(Phase0BridgeError, match="missing or unknown"):
        driver._worker_burst({"request_id": "worker-0"})  # noqa: SLF001
    assert (candidate / "lib.rs").read_bytes() == b"original\n"
    assert not (authority / "active-worker-burst.json").exists()
    assert list((authority / "worker-snapshots").iterdir()) == []


def test_startup_recovers_interrupted_worker_before_any_new_burst(tmp_path: Path) -> None:
    driver, candidate, authority, state_path = _driver_fixture(
        tmp_path, lambda _request, _environment: {}
    )
    snapshot = authority / "worker-snapshots" / "interrupted"
    snapshot.mkdir()
    (snapshot / "lib.rs").write_bytes(b"original\n")
    (candidate / "lib.rs").write_bytes(b"dirty interrupted write\n")
    journal = {
        "schema": "trellis-phase0-worker-journal/v1",
        "base_state_sha256": _sha(b"state-0"),
        "candidate_path": str(candidate.resolve()),
        "snapshot_path": str(snapshot.resolve()),
    }
    (authority / "active-worker-burst.json").write_bytes(canonical_json(journal))
    recovered = Phase0Driver(
        kernel=driver.kernel,
        broker=driver.broker,
        attempt_root=driver.attempt,
        worker=driver.worker,
        reviewer=driver.reviewer,
    )
    assert recovered.kernel.state == state_path.resolve()
    assert (candidate / "lib.rs").read_bytes() == b"original\n"
    assert not snapshot.exists()
    assert not (authority / "active-worker-burst.json").exists()


def test_startup_keeps_revision_when_kernel_transition_already_landed(tmp_path: Path) -> None:
    driver, candidate, authority, state_path = _driver_fixture(
        tmp_path, lambda _request, _environment: {}
    )
    snapshot = authority / "worker-snapshots" / "pre-transition"
    snapshot.mkdir()
    (snapshot / "lib.rs").write_bytes(b"original\n")
    (candidate / "lib.rs").write_bytes(b"accepted revision\n")
    state_path.write_bytes(canonical_json({"generation": 1, "state_sha256": _sha(b"state-1")}))
    (authority / "active-worker-burst.json").write_bytes(
        canonical_json(
            {
                "schema": "trellis-phase0-worker-journal/v1",
                "base_state_sha256": _sha(b"state-0"),
                "candidate_path": str(candidate.resolve()),
                "snapshot_path": str(snapshot.resolve()),
            }
        )
    )
    Phase0Driver(
        kernel=driver.kernel,
        broker=driver.broker,
        attempt_root=driver.attempt,
        worker=driver.worker,
        reviewer=driver.reviewer,
    )
    assert (candidate / "lib.rs").read_bytes() == b"accepted revision\n"
    assert not snapshot.exists()
    assert not (authority / "active-worker-burst.json").exists()


def test_transport_retries_do_not_advance_semantic_handbacks() -> None:
    status = {"stage": "check", "phase0_handbacks": 0}

    class RetryDriver(Phase0Driver):
        calls = 0

        def step(self):
            self.calls += 1
            if self.calls < 3:
                raise Phase0BridgeError("transport")
            status["stage"] = "incomplete"

    driver = object.__new__(RetryDriver)
    driver.kernel = SimpleNamespace(check=lambda: dict(status))
    result = driver.run()
    assert driver.calls == 3
    assert result["phase0_handbacks"] == 0


def test_kernel_rejection_is_not_retried_as_transport(capsys) -> None:
    status = {"stage": "worker", "phase0_handbacks": 0}

    class RejectingDriver(Phase0Driver):
        calls = 0

        def step(self):
            self.calls += 1
            raise Phase0KernelRejection("kernel command phase0-apply failed (2): ledger replay")

    driver = object.__new__(RejectingDriver)
    driver.kernel = SimpleNamespace(check=lambda: dict(status))
    with pytest.raises(Phase0KernelRejection, match="ledger replay"):
        driver.run()
    assert driver.calls == 1


def test_transport_retries_are_logged_to_stderr(capsys) -> None:
    status = {"stage": "check", "phase0_handbacks": 0}

    class RetryDriver(Phase0Driver):
        calls = 0

        def step(self):
            self.calls += 1
            if self.calls < 2:
                raise Phase0BridgeError("agent transport failed: codex exited 1")
            status["stage"] = "incomplete"

    driver = object.__new__(RetryDriver)
    driver.kernel = SimpleNamespace(check=lambda: dict(status))
    driver.run()
    err = capsys.readouterr().err
    assert "re-dispatching (1/3)" in err and "codex exited 1" in err


def test_environmental_retries_have_a_separate_bound_and_no_handback_charge() -> None:
    status = {"stage": "check", "phase0_handbacks": 0}

    class RetryDriver(Phase0Driver):
        calls = 0

        def step(self):
            self.calls += 1
            if self.calls < 3:
                raise Phase0EnvironmentalFailure("recorded timeout")
            status["stage"] = "incomplete"

    driver = object.__new__(RetryDriver)
    driver.kernel = SimpleNamespace(check=lambda: dict(status))
    driver.retained_final_result = None
    result = driver.run()
    assert driver.calls == 3
    assert result["phase0_handbacks"] == 0


def test_agent_delivery_may_use_any_json_formatting(tmp_path: Path) -> None:
    pretty = tmp_path / "pretty.json"
    pretty.write_text('{\n  "b": [1, 2],\n  "a": {"z": true, "y": null}\n}\n', encoding="utf-8")
    assert _read_agent_json(pretty) == {"b": [1, 2], "a": {"z": True, "y": None}}
    dup = tmp_path / "dup.json"
    dup.write_text('{"a": 1, "a": 2}', encoding="utf-8")
    with pytest.raises(Phase0DraftRejected, match="duplicate JSON member"):
        _read_agent_json(dup)
    array = tmp_path / "array.json"
    array.write_text("[1]", encoding="utf-8")
    with pytest.raises(Phase0DraftRejected, match="one JSON object"):
        _read_agent_json(array)


def test_seal_rejects_affected_targets_outside_the_request_goal_targets(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    candidate = tmp_path / "candidate"
    raw.mkdir()
    candidate.mkdir()
    source = b"pub fn target() {}\n"
    (raw / "lib.rs").write_bytes(source)
    (candidate / "lib.rs").write_bytes(source)
    cargo = b"[package]\nname='fixture'\n"
    (candidate / "Cargo.toml").write_bytes(cargo)
    receipt = _sha(b"discovery")
    request = {
        "request_id": "worker-0",
        "request_sha256": _sha(b"request"),
        "goal_targets": ["goal:compute", "goal:parse"],
    }
    state = {
        "generation": 0,
        "genesis": {"unadapted_source_manifest": source_tree_manifest(raw), "goal_sha256": _sha(b"goal")},
    }
    def draft_with(targets):
        draft = _draft("worker-0", receipt, cargo)
        draft["entries"][0]["affected_targets"] = targets
        return draft

    with pytest.raises(Phase0DraftRejected, match="goal:not_a_target.*goal_targets") as info:
        seal_worker_draft(draft_with(["compute", "goal:parse", "goal:not_a_target"]), request, state, candidate)
    assert "'compute'" in str(info.value)
    sealed = seal_worker_draft(draft_with(["goal:parse", "goal:compute"]), request, state, candidate)
    assert sealed["ledger"]["entries"][0]["affected_targets"] == ["goal:compute", "goal:parse"]
    with pytest.raises(Phase0DraftRejected, match="non-empty"):
        seal_worker_draft(draft_with([]), request, state, candidate)


def test_rejected_delivery_reason_is_fed_back_to_the_next_dispatch(tmp_path: Path) -> None:
    calls: list[dict] = []

    def worker(_request, _environment, rejection=None):
        calls.append({"rejection": rejection})
        return {"malformed": True}

    driver, _candidate, _authority, _state = _driver_fixture(tmp_path, worker)
    with pytest.raises(Phase0DraftRejected, match="missing or unknown"):
        driver._worker_burst({"request_id": "worker-0"})  # noqa: SLF001
    assert calls[0]["rejection"] is None
    assert "missing or unknown" in driver.last_draft_rejection
    with pytest.raises(Phase0DraftRejected):
        driver._worker_burst({"request_id": "worker-0"})  # noqa: SLF001
    assert "missing or unknown" in calls[1]["rejection"]


def test_dispatch_prompt_appends_rejection_after_the_unchanged_contract(tmp_path: Path, monkeypatch) -> None:
    attempt = tmp_path / "attempt"
    attempt.mkdir()
    dispatcher = ExistingLaneDispatcher(
        attempt, {"bindings": {"worker": {"binding_sha256": _sha(b"worker")}}},
    )
    seen: dict = {}

    class Response:
        ok = True
        error = ""

    def fake_execute(single, validate_artifact):
        seen["prompt"] = single.prompt
        raw = Path(single.state_dir) / "staging" / "req-1.raw.json"
        raw.parent.mkdir(parents=True, exist_ok=True)
        raw.write_text('{\n "ok": true\n}\n')
        return Response()

    monkeypatch.setattr("scripts.pv_phase0.execute_agent_request", fake_execute)
    monkeypatch.setattr(
        "scripts.pv_phase0.seed_burst_home",
        lambda root, name, persistent: (root / "burst-homes" / name),
    )
    monkeypatch.setattr(
        "scripts.pv_phase0.prepare_artifact_paths",
        lambda output, attempt_root, name: SimpleNamespace(
            raw=output / "staging" / "req-1.raw.json", done=output / "staging" / "req-1.done"
        ),
    )
    request = {"request_id": "req-1", "generation": 0}
    binding = {
        "provider": "codex", "model": "m", "effort": "low", "extra_args": [],
        "fallback_models": [], "lane_id": "worker", "binding_sha256": _sha(b"worker"),
    }
    fragment = "pv/source_adaptation/worker/05_role.md"
    out = dispatcher._dispatch(  # noqa: SLF001
        request=request, binding=binding, fragment=fragment, role="source_adaptation_worker",
        environment={}, persistent=True, rejection=None,
    )
    assert out == {"ok": True}  # pretty-printed delivery accepted
    base = seen["prompt"]
    assert "(any formatting)" in base
    dispatcher._dispatch(  # noqa: SLF001
        request=request, binding=binding, fragment=fragment, role="source_adaptation_worker",
        environment={}, persistent=True, rejection="affected_targets names ['x']",
    )
    assert seen["prompt"].startswith(base)
    assert "previous delivery for this request was rejected: affected_targets names ['x']" in seen["prompt"]
    assert "restored to its state before that attempt" in seen["prompt"]


def test_bridge_prompt_contract_version_matches_the_kernel() -> None:
    source = (ROOT / "kernel" / "src" / "request_contracts.rs").read_text(encoding="utf-8")
    body = source[source.index("pub fn prompt_contract_version() -> u32 {"):]
    body = body[: body.index("\n}")]
    kernel_value = int(body.rstrip().rsplit(None, 1)[-1])
    assert PHASE0_PROMPT_CONTRACT_VERSION == kernel_value


def test_seal_rejects_malformed_span_shapes_with_a_reason(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    candidate = tmp_path / "candidate"
    raw.mkdir()
    candidate.mkdir()
    source = b"pub fn target() {}\n"
    (raw / "lib.rs").write_bytes(source)
    (candidate / "lib.rs").write_bytes(source)
    cargo = b"[package]\nname='fixture'\n"
    (candidate / "Cargo.toml").write_bytes(cargo)
    receipt = _sha(b"discovery")
    request = {"request_id": "worker-0", "request_sha256": _sha(b"request")}
    state = {
        "generation": 0,
        "genesis": {"unadapted_source_manifest": source_tree_manifest(raw), "goal_sha256": _sha(b"goal")},
    }
    draft = _draft("worker-0", receipt, cargo)
    draft["entries"][0]["before_span"] = [0, 0]  # the live run-6 shape: a list, not an object
    with pytest.raises(Phase0DraftRejected, match="before_span must be an object"):
        seal_worker_draft(draft, request, state, candidate)
    draft = _draft("worker-0", receipt, cargo)
    draft["entries"][0]["after_span"] = {"start": 10, "end": 2}
    with pytest.raises(Phase0DraftRejected, match="start <= end"):
        seal_worker_draft(draft, request, state, candidate)
    draft = _draft("worker-0", receipt, cargo)
    draft["entries"][0]["citation"] = "see the Cargo book"
    with pytest.raises(Phase0DraftRejected, match="citation must be"):
        seal_worker_draft(draft, request, state, candidate)
    draft = _draft("worker-0", receipt, cargo)
    draft["entries"][0]["quoted_error"]["byte_offset"] = "0"
    with pytest.raises(Phase0DraftRejected, match="quoted_error needs"):
        seal_worker_draft(draft, request, state, candidate)


def test_unexpected_seal_exception_becomes_draft_feedback(tmp_path: Path, monkeypatch) -> None:
    driver, _candidate, _authority, _state = _driver_fixture(tmp_path, lambda _r, _e, rejection=None: {"x": 1})

    def exploding(*_args, **_kwargs):
        raise TypeError("list indices must be integers or slices, not str")

    monkeypatch.setattr("scripts.pv_phase0.seal_worker_draft", exploding)
    with pytest.raises(Phase0DraftRejected, match="malformed field \\(TypeError"):
        driver._worker_burst({"request_id": "worker-0"})  # noqa: SLF001
    assert "TypeError" in driver.last_draft_rejection


def test_seal_carries_forward_untouched_prior_entries_and_binds_the_thread_id(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    candidate = tmp_path / "candidate"
    raw.mkdir()
    candidate.mkdir()
    source = b"pub fn target() {}\n"
    (raw / "lib.rs").write_bytes(source)
    (candidate / "lib.rs").write_bytes(source)
    cargo = b"[package]\nname='fixture'\n"
    (candidate / "Cargo.toml").write_bytes(cargo)
    receipt = _sha(b"discovery")
    request = {"request_id": "worker-0", "request_sha256": _sha(b"request"), "worker_binding_sha256": "ab" * 32}
    state = {
        "generation": 0,
        "genesis": {"unadapted_source_manifest": source_tree_manifest(raw), "goal_sha256": _sha(b"goal")},
    }
    first = seal_worker_draft(_draft("worker-0", receipt, cargo), request, state, candidate)
    assert first["worker_thread_id"] == "phase0-worker-" + "ab" * 8  # bridge-derived, not the draft's
    prior_entry = first["ledger"]["entries"][0]

    # Generation 1: the worker adds Cargo.lock and describes only that file.
    lock = b"version = 4\n"
    (candidate / "Cargo.lock").write_bytes(lock)
    state1 = {
        "generation": 1,
        "worker_thread_id": first["worker_thread_id"],
        "ledger": first["ledger"],
        "genesis": state["genesis"],
    }
    request1 = {"request_id": "worker-1", "request_sha256": _sha(b"request-1"), "worker_binding_sha256": "ab" * 32}
    draft = _draft("worker-1", _sha(b"lock-discovery"), lock)
    draft["worker_thread_id"] = "some-other-codex-thread"
    draft["entries"][0]["file"] = "Cargo.lock"
    second = seal_worker_draft(draft, request1, state1, candidate)
    assert second["worker_thread_id"] == first["worker_thread_id"]
    files = [(e["file"], e["id"]) for e in second["ledger"]["entries"]]
    assert [f for f, _ in files] == ["Cargo.lock", "Cargo.toml"]
    # carried forward with its diff digest intact and its id re-bound to generation 2
    carried = [e for e in second["ledger"]["entries"] if e["file"] == "Cargo.toml"][0]
    assert carried["diff_sha256"] == prior_entry["diff_sha256"]
    assert carried["id"] == f"phase0-2-{prior_entry['diff_sha256']}"
    assert carried["patch"] == prior_entry["patch"] and carried["quoted_error"] == prior_entry["quoted_error"]
    assert second["ledger"]["generation"] == 2


def test_seal_requires_the_quote_to_be_one_complete_receipt_line(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    candidate = tmp_path / "candidate"
    raw.mkdir()
    candidate.mkdir()
    source = b"pub fn target() {}\n"
    (raw / "lib.rs").write_bytes(source)
    (candidate / "lib.rs").write_bytes(source)
    cargo = b"[package]\nname='fixture'\n"
    (candidate / "Cargo.toml").write_bytes(cargo)
    stderr = b"standalone Cargo.toml is unavailable: No such file or directory\nsecond line\n"
    receipt_sha = _sha(b"discovery")
    receipt = {
        "receipt_sha256": receipt_sha,
        "parsed_stdout": {"stages": [{"name": "standalone_package", "stderr_base64": base64.b64encode(stderr).decode("ascii")}]},
    }
    request = {"request_id": "worker-0", "request_sha256": _sha(b"request"), "checker_failure_receipt": receipt}
    state = {
        "generation": 0,
        "genesis": {"unadapted_source_manifest": source_tree_manifest(raw), "goal_sha256": _sha(b"goal")},
    }

    def draft_quoting(text: str, offset: int = 0):
        draft = _draft("worker-0", receipt_sha, cargo)
        draft["entries"][0]["quoted_error"]["quote"] = text
        draft["entries"][0]["quoted_error"]["byte_offset"] = offset
        return draft

    sealed = seal_worker_draft(draft_quoting("standalone Cargo.toml is unavailable: No such file or directory"), request, state, candidate)
    assert sealed["ledger"]["entries"][0]["quoted_error"]["quote"].endswith("directory")
    with pytest.raises(Phase0DraftRejected, match="one complete line.*the line at offset 0 is: standalone Cargo.toml is unavailable"):
        seal_worker_draft(draft_quoting("standalone Cargo.toml is unavailable"), request, state, candidate)
    # A wrong offset for a quote that does occur as a complete line is snapped, not refused.
    snapped = seal_worker_draft(draft_quoting("second line", offset=3), request, state, candidate)
    assert snapped["ledger"]["entries"][0]["quoted_error"]["byte_offset"] == len(stderr) - 12
    sealed = seal_worker_draft(draft_quoting("second line", offset=len(stderr) - len(b"second line\n")), request, state, candidate)
    assert sealed["ledger"]["entries"][0]["quoted_error"]["byte_offset"] == len(stderr) - 12


def test_seal_computes_span_digests_from_the_patch_bytes(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    candidate = tmp_path / "candidate"
    raw.mkdir()
    candidate.mkdir()
    before = b"#![unstable]\npub fn target() {}\n"
    (raw / "lib.rs").write_bytes(before)
    (candidate / "lib.rs").write_bytes(b"pub fn target() {}\n")
    receipt = _sha(b"discovery")
    request = {"request_id": "worker-0", "request_sha256": _sha(b"request")}
    state = {
        "generation": 0,
        "genesis": {"unadapted_source_manifest": source_tree_manifest(raw), "goal_sha256": _sha(b"goal")},
    }
    draft = _draft("worker-0", receipt, b"")
    entry = draft["entries"][0]
    entry.update({
        "file": "lib.rs", "operation": "modify",
        "before_span": {"start": 0, "end": 13}, "after_span": {"start": 0, "end": 0},
        "before_sha256": None, "after_sha256": None,  # the worker may leave these null
        "patch": {"removed_base64": base64.b64encode(b"#![unstable]\n").decode("ascii"), "inserted_base64": ""},
        "seam_class": "i",
    })
    sealed = seal_worker_draft(draft, request, state, candidate)
    got = sealed["ledger"]["entries"][0]
    assert got["before_sha256"] == _sha(before)  # whole unadapted file
    assert got["after_sha256"] == _sha(b"pub fn target() {}\n")  # whole candidate file


def test_seal_carries_a_prior_modify_when_a_second_span_of_the_same_file_changes(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    candidate = tmp_path / "candidate"
    raw.mkdir()
    candidate.mkdir()
    original = b"#![unstable]\nuse crate::num;\npub fn target() {}\n"
    (raw / "lib.rs").write_bytes(original)
    receipt = _sha(b"discovery")
    request = {"request_id": "worker-0", "request_sha256": _sha(b"request"), "worker_binding_sha256": "ab" * 32}
    state = {"generation": 0, "genesis": {"unadapted_source_manifest": source_tree_manifest(raw), "goal_sha256": _sha(b"goal")}}

    def modify(draft_id, before_start, before_end, removed, inserted, after_start, after_end):
        d = _draft(draft_id, receipt, b"")
        e = d["entries"][0]
        e.update({
            "file": "lib.rs", "operation": "modify", "seam_class": "i",
            "before_span": {"start": before_start, "end": before_end},
            "after_span": {"start": after_start, "end": after_end},
            "before_sha256": None, "after_sha256": None,
            "patch": {"removed_base64": base64.b64encode(removed).decode("ascii"), "inserted_base64": base64.b64encode(inserted).decode("ascii")},
        })
        return d

    # Generation 1: remove the first line.
    (candidate / "lib.rs").write_bytes(b"use crate::num;\npub fn target() {}\n")
    first = seal_worker_draft(modify("worker-0", 0, 13, b"#![unstable]\n", b"", 0, 0), request, state, candidate)
    assert [e["file"] for e in first["ledger"]["entries"]] == ["lib.rs"]
    # Generation 2: a second, non-overlapping span of the same file.
    (candidate / "lib.rs").write_bytes(b"use core::num;\npub fn target() {}\n")
    state1 = {"generation": 1, "worker_thread_id": first["worker_thread_id"], "ledger": first["ledger"], "genesis": state["genesis"]}
    request1 = {"request_id": "worker-1", "request_sha256": _sha(b"request-1"), "worker_binding_sha256": "ab" * 32}
    second = seal_worker_draft(modify("worker-1", 17, 22, b"crate", b"core", 0, 0), request1, state1, candidate)
    rows = sorted(second["ledger"]["entries"], key=lambda e: e["before_span"]["start"])
    assert [(e["before_span"]["start"], e["before_span"]["end"]) for e in rows] == [(0, 13), (17, 22)]  # carried, not dropped
    # after_spans are rebased by the bridge over the earlier span (-13 bytes): "crate" sat at 17..22, now 4..8 after "core"
    assert [(e["after_span"]["start"], e["after_span"]["end"]) for e in rows] == [(0, 0), (4, 8)]
    for e in rows:
        assert e["before_sha256"] == _sha(original)
        assert e["after_sha256"] == _sha(b"use core::num;\npub fn target() {}\n")  # refreshed on the carried entry too
    # identity (diff, hence id modulo generation) of the carried entry is unchanged by the second seam
    assert rows[0]["diff_sha256"] == first["ledger"]["entries"][0]["diff_sha256"]
    # Re-specifying the same original span replaces the prior entry instead of duplicating it.
    (candidate / "lib.rs").write_bytes(b"// gone\nuse crate::num;\npub fn target() {}\n")
    third = seal_worker_draft(modify("worker-1", 0, 13, b"#![unstable]\n", b"// gone\n", 0, 8), request1, state1, candidate)
    assert [(e["before_span"]["start"], e["before_span"]["end"]) for e in third["ledger"]["entries"]] == [(0, 13)]


def _modify_draft(request_id, receipt, file, start, end, removed, inserted):
    d = _draft(request_id, receipt, b"")
    e = d["entries"][0]
    e.update({
        "file": file, "operation": "modify", "seam_class": "i",
        "before_span": {"start": start, "end": end}, "after_span": {"start": 0, "end": 0},
        "before_sha256": None, "after_sha256": None,
        "patch": {"removed_base64": base64.b64encode(removed).decode("ascii"),
                  "inserted_base64": base64.b64encode(inserted).decode("ascii")},
    })
    return d


def test_seal_translates_candidate_coordinates_when_that_replays_exactly(tmp_path: Path, capsys) -> None:
    raw = tmp_path / "raw"
    candidate = tmp_path / "candidate"
    raw.mkdir()
    candidate.mkdir()
    original = b"#![unstable]\nuse crate::num;\npub fn target() {}\n"
    (raw / "lib.rs").write_bytes(original)
    receipt = _sha(b"discovery")
    request = {"request_id": "worker-0", "request_sha256": _sha(b"request"), "worker_binding_sha256": "ab" * 32}
    state = {"generation": 0, "genesis": {"unadapted_source_manifest": source_tree_manifest(raw), "goal_sha256": _sha(b"goal")}}
    # Generation 1 removes the first line (13 bytes).
    (candidate / "lib.rs").write_bytes(b"use crate::num;\npub fn target() {}\n")
    first = seal_worker_draft(_modify_draft("worker-0", receipt, "lib.rs", 0, 13, b"#![unstable]\n", b""), request, state, candidate, unadapted=raw)
    state1 = {"generation": 1, "worker_thread_id": first["worker_thread_id"], "ledger": first["ledger"], "genesis": state["genesis"]}
    request1 = {"request_id": "worker-1", "request_sha256": _sha(b"request-1"), "worker_binding_sha256": "ab" * 32}
    # Generation 2 inserts a shim module before `pub fn`, but the worker measured
    # the insertion point in the CANDIDATE (offset 16) rather than the uploaded file (29).
    shim = b"mod num {}\n"
    (candidate / "lib.rs").write_bytes(b"use crate::num;\n" + shim + b"pub fn target() {}\n")
    second = seal_worker_draft(_modify_draft("worker-1", receipt, "lib.rs", 16, 16, b"", shim), request1, state1, candidate, unadapted=raw)
    rows = sorted(second["ledger"]["entries"], key=lambda e: e["before_span"]["start"])
    assert [(e["before_span"]["start"], e["before_span"]["end"]) for e in rows] == [(0, 13), (29, 29)]
    assert rows[1]["after_span"] == {"start": 16, "end": 16 + len(shim)}
    assert rows[1]["id"] == f"phase0-2-{rows[1]['diff_sha256']}"
    assert "translated before_span" in capsys.readouterr().err
    # A genuinely wrong patch is refused with the bytes actually found.
    (candidate / "lib.rs").write_bytes(b"use crate::num;\nmod other {}\npub fn target() {}\n")
    with pytest.raises(Phase0DraftRejected, match="do not reproduce the candidate"):
        seal_worker_draft(_modify_draft("worker-1", receipt, "lib.rs", 29, 29, b"", shim), request1, state1, candidate, unadapted=raw)
    with pytest.raises(Phase0DraftRejected, match="holds b'use c'.*not the declared removed bytes"):
        seal_worker_draft(_modify_draft("worker-1", receipt, "lib.rs", 13, 18, b"crate", b"core"), request1, state1, candidate, unadapted=raw)


def _add_delete_fixture(tmp_path: Path) -> tuple[Path, Path, str, dict, dict]:
    raw = tmp_path / "raw"
    candidate = tmp_path / "candidate"
    raw.mkdir()
    candidate.mkdir()
    source = b"pub fn target() {}\n"
    (raw / "lib.rs").write_bytes(source)
    (candidate / "lib.rs").write_bytes(source)
    receipt = _sha(b"discovery")
    request = {"request_id": "worker-0", "request_sha256": _sha(b"request")}
    state = {
        "generation": 0,
        "genesis": {
            "unadapted_source_manifest": source_tree_manifest(raw),
            "goal_sha256": _sha(b"goal"),
        },
    }
    return raw, candidate, receipt, request, state


def test_seal_rejects_an_add_the_candidate_tree_does_not_carry(tmp_path: Path) -> None:
    raw, candidate, receipt, request, state = _add_delete_fixture(tmp_path)
    cargo = b"[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n"

    # The entry describes the add, but the file was never written to the tree
    # the framework will manifest.
    with pytest.raises(Phase0DraftRejected) as absent:
        seal_worker_draft(
            _draft("worker-0", receipt, cargo), request, state, candidate, unadapted=raw
        )
    assert "'Cargo.toml'" in str(absent.value)
    assert "the edit was not made in the candidate tree" in str(absent.value)
    assert isinstance(absent.value, Phase0BridgeError)

    # Present, but not the bytes the patch inserts.
    (candidate / "Cargo.toml").write_bytes(cargo + b"# drifted\n")
    with pytest.raises(Phase0DraftRejected, match="the candidate tree holds"):
        seal_worker_draft(
            _draft("worker-0", receipt, cargo), request, state, candidate, unadapted=raw
        )

    (candidate / "Cargo.toml").write_bytes(cargo)
    response = seal_worker_draft(
        _draft("worker-0", receipt, cargo), request, state, candidate, unadapted=raw
    )
    assert [entry["file"] for entry in response["ledger"]["entries"]] == ["Cargo.toml"]


def _delete_draft(request_id: str, receipt: str, file: str, removed: bytes) -> dict:
    draft = _draft(request_id, receipt, b"")
    draft["entries"][0].update(
        {
            "file": file,
            "operation": "delete",
            "seam_class": "i",
            "before_span": {"start": 0, "end": len(removed)},
            "after_span": {"start": 0, "end": 0},
            "patch": {
                "removed_base64": base64.b64encode(removed).decode("ascii"),
                "inserted_base64": "",
            },
        }
    )
    return draft


def test_seal_rejects_a_delete_whose_file_is_still_in_the_candidate_tree(tmp_path: Path) -> None:
    raw, candidate, receipt, request, state = _add_delete_fixture(tmp_path)
    legacy = b"pub fn legacy() {}\n"
    (raw / "legacy.rs").write_bytes(legacy)
    (candidate / "legacy.rs").write_bytes(legacy)
    state["genesis"]["unadapted_source_manifest"] = source_tree_manifest(raw)
    draft = _delete_draft("worker-0", receipt, "legacy.rs", legacy)

    with pytest.raises(Phase0DraftRejected) as still_there:
        seal_worker_draft(draft, request, state, candidate, unadapted=raw)
    assert "'legacy.rs'" in str(still_there.value)
    assert "the edit was not made in the candidate tree" in str(still_there.value)

    (candidate / "legacy.rs").unlink()
    response = seal_worker_draft(
        _delete_draft("worker-0", receipt, "legacy.rs", legacy), request, state, candidate,
        unadapted=raw,
    )
    entry = response["ledger"]["entries"][0]
    assert entry["operation"] == "delete" and entry["after_sha256"] is None


def test_seal_keeps_the_existing_modify_diagnostics_for_a_missing_file(tmp_path: Path) -> None:
    raw, candidate, receipt, request, state = _add_delete_fixture(tmp_path)
    original = (raw / "lib.rs").read_bytes()

    # A modify of a file the candidate no longer carries, and a modify of a
    # file that was never uploaded, keep their own reasons.
    (candidate / "lib.rs").unlink()
    with pytest.raises(Phase0DraftRejected, match="a modify needs the file in the candidate"):
        seal_worker_draft(
            _modify_draft("worker-0", receipt, "lib.rs", 0, 3, b"pub", b"pvt"),
            request, state, candidate, unadapted=raw,
        )
    (candidate / "lib.rs").write_bytes(original)
    with pytest.raises(Phase0DraftRejected, match="modify needs the file in the upload"):
        seal_worker_draft(
            _modify_draft("worker-0", receipt, "absent.rs", 0, 0, b"", b"x"),
            request, state, candidate, unadapted=raw,
        )

    (candidate / "lib.rs").write_bytes(b"pvt fn target() {}\n")
    response = seal_worker_draft(
        _modify_draft("worker-0", receipt, "lib.rs", 0, 3, b"pub", b"pvt"),
        request, state, candidate, unadapted=raw,
    )
    assert response["ledger"]["entries"][0]["operation"] == "modify"


def test_draft_rejections_have_their_own_wider_bound() -> None:
    status = {"stage": "worker", "phase0_handbacks": 0}

    class RejectingDriver(Phase0Driver):
        calls = 0

        def step(self):
            self.calls += 1
            if self.calls < 5:
                raise Phase0DraftRejected(f"slip {self.calls}")
            status["stage"] = "incomplete"

    driver = object.__new__(RejectingDriver)
    driver.kernel = SimpleNamespace(check=lambda: dict(status))
    driver.retained_final_result = None
    assert driver.run()["stage"] == "incomplete"
    assert driver.calls == 5  # four rejections, fifth attempt succeeds

    class HopelessDriver(Phase0Driver):
        calls = 0

        def step(self):
            self.calls += 1
            raise Phase0DraftRejected("never right")

    hopeless = object.__new__(HopelessDriver)
    hopeless.kernel = SimpleNamespace(check=lambda: dict(status | {"stage": "worker"}))
    with pytest.raises(Phase0DraftRejected):
        hopeless.run()
    assert hopeless.calls == 5


def test_seal_fills_the_quoted_tool_from_the_receipt_stage(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    candidate = tmp_path / "candidate"
    raw.mkdir()
    candidate.mkdir()
    (raw / "lib.rs").write_bytes(b"pub fn target() {}\n")
    (candidate / "lib.rs").write_bytes(b"pub fn target() {}\n")
    cargo = b"[package]\nname='fixture'\n"
    (candidate / "Cargo.toml").write_bytes(cargo)
    stderr = b"standalone Cargo.toml is unavailable: No such file or directory\n"
    receipt_sha = _sha(b"discovery")
    receipt = {"receipt_sha256": receipt_sha, "parsed_stdout": {"stages": [
        {"name": "standalone_package", "tool": "phase0-checker", "stderr_base64": base64.b64encode(stderr).decode("ascii")}]}}
    request = {"request_id": "worker-0", "request_sha256": _sha(b"request"), "checker_failure_receipt": receipt}
    state = {"generation": 0, "genesis": {"unadapted_source_manifest": source_tree_manifest(raw), "goal_sha256": _sha(b"goal")}}
    draft = _draft("worker-0", receipt_sha, cargo)
    draft["entries"][0]["quoted_error"]["quote"] = "standalone Cargo.toml is unavailable: No such file or directory"
    draft["entries"][0]["quoted_error"]["tool"] = "aeneas"  # wrong; the stage says phase0-checker
    sealed = seal_worker_draft(draft, request, state, candidate)
    assert sealed["ledger"]["entries"][0]["quoted_error"]["tool"] == "phase0-checker"


def test_seal_snaps_a_one_byte_early_offset_and_reads_state_receipt_records(tmp_path: Path, capsys) -> None:
    raw = tmp_path / "raw"
    candidate = tmp_path / "candidate"
    raw.mkdir()
    candidate.mkdir()
    (raw / "lib.rs").write_bytes(b"pub fn target() {}\n")
    (candidate / "lib.rs").write_bytes(b"pub fn target() {}\n")
    cargo = b"[package]\nname='fixture'\n"
    (candidate / "Cargo.toml").write_bytes(cargo)
    stream = b"progress\n[Error] Could not match the contexts\nmore\n[Error] Could not match the contexts\n"
    receipt_sha = _sha(b"older-authoritative")
    inner = {"receipt_sha256": receipt_sha, "parsed_stdout": {"stages": [
        {"name": "aeneas", "tool": "aeneas", "stdout_base64": base64.b64encode(stream).decode("ascii")}]}}
    # Known only through the kernel state, in its evidence-record shape.
    state = {
        "generation": 3,
        "checker_receipts": [{"receipt_sha256": receipt_sha, "receipt": inner}],
        "genesis": {"unadapted_source_manifest": source_tree_manifest(raw), "goal_sha256": _sha(b"goal")},
    }
    request = {"request_id": "worker-3", "request_sha256": _sha(b"request")}
    def draft_at(offset, quote="[Error] Could not match the contexts"):
        d = _draft("worker-3", receipt_sha, cargo)
        d["entries"][0]["quoted_error"].update({"quote": quote, "stream": "stdout", "byte_offset": offset, "tool": "x"})
        return d

    sealed = seal_worker_draft(draft_at(8), request, state, candidate)  # 8 is the newline; 9 is the line
    got = sealed["ledger"]["entries"][0]["quoted_error"]
    assert got["byte_offset"] == 9 and got["tool"] == "aeneas"
    assert "snapped quoted_error.byte_offset 8 -> 9" in capsys.readouterr().err
    # nearer the second occurrence (at 51) than the first (at 9)
    assert seal_worker_draft(draft_at(60), request, state, candidate)["ledger"]["entries"][0]["quoted_error"]["byte_offset"] == 51
    with pytest.raises(Phase0DraftRejected, match="does not occur as a complete line"):
        seal_worker_draft(draft_at(9, quote="[Error] not in the stream"), request, state, candidate)


def test_seal_reads_the_request_failure_receipt_as_an_evidence_record(tmp_path: Path, capsys) -> None:
    """The request carries the generation's own failure as {receipt_sha256,
    receipt}, the same shape kernel state uses. Read as a wrapper it exposes no
    stream, and every quote check passes without checking anything."""
    raw = tmp_path / "raw"
    candidate = tmp_path / "candidate"
    raw.mkdir()
    candidate.mkdir()
    (raw / "lib.rs").write_bytes(b"pub fn target() {}\n")
    (candidate / "lib.rs").write_bytes(b"pub fn target() {}\n")
    cargo = b"[package]\nname='fixture'\n"
    (candidate / "Cargo.toml").write_bytes(cargo)
    stream = b"progress\n[Error] Could not match the contexts\n"
    receipt_sha = _sha(b"this-generation")
    inner = {"receipt_sha256": receipt_sha, "parsed_stdout": {"stages": [
        {"name": "aeneas", "tool": "aeneas", "stdout_base64": base64.b64encode(stream).decode("ascii")}]}}
    state = {
        "generation": 3,
        "checker_receipts": [],
        "genesis": {"unadapted_source_manifest": source_tree_manifest(raw), "goal_sha256": _sha(b"goal")},
    }
    request = {
        "request_id": "worker-3",
        "request_sha256": _sha(b"request"),
        "checker_failure_receipt": {"receipt_sha256": receipt_sha, "receipt": inner},
    }
    draft = _draft("worker-3", receipt_sha, cargo)
    # 8 is the newline before the line, which starts at 9.
    draft["entries"][0]["quoted_error"].update(
        {"quote": "[Error] Could not match the contexts", "stream": "stdout", "byte_offset": 8, "tool": "x"}
    )
    sealed = seal_worker_draft(draft, request, state, candidate)
    got = sealed["ledger"]["entries"][0]["quoted_error"]
    assert got["byte_offset"] == 9 and got["tool"] == "aeneas"
    assert "snapped quoted_error.byte_offset 8 -> 9" in capsys.readouterr().err


def test_genesis_components_match_the_kernel_closure_and_are_all_verified() -> None:
    """Genesis is parsed into a struct that denies unknown fields and digest
    checks a list of them, so a component the bridge writes must exist in that
    struct and appear in that list. A key on one side only kills every run."""
    repo_root = Path(__file__).resolve().parents[1]
    kernel_source = (repo_root / "kernel/src/phase0.rs").read_text(encoding="utf-8")
    closure = re.search(
        r"pub struct Phase0ComponentClosure \{(.*?)\n\}", kernel_source, re.S
    )
    assert closure, "the component closure struct moved"
    declared = set(re.findall(r"\n    pub (\w+): Phase0PinnedFile,", closure.group(1)))
    verified = set(
        re.findall(
            r"&components\.(\w+),",
            re.search(
                r"fn component_files\(components: &Phase0ComponentClosure\)"
                r" -> Vec<&Phase0PinnedFile> \{(.*?)\n\}",
                kernel_source,
                re.S,
            ).group(1),
        )
    )
    bridge_source = (repo_root / "scripts/pv_phase0.py").read_text(encoding="utf-8")
    literal = re.search(r"\n    components = \{(.*?)\n    \}", bridge_source, re.S)
    assert literal, "the components literal moved"
    written = set(re.findall(r'"(\w+)": _pin_file\(', literal.group(1))) | set(
        re.findall(r'components\["(\w+)"\] = _pin_file\(', bridge_source)
    )
    assert written == declared, (
        "the bridge and the kernel disagree about the pinned components: "
        f"bridge only {sorted(written - declared)}, kernel only {sorted(declared - written)}"
    )
    assert declared == verified, (
        "a pinned component is recorded but never digest checked: "
        f"{sorted(declared - verified)}"
    )


def _history_entry(
    path: str,
    operation: str,
    start: int,
    end: int,
    inserted: bytes = b"",
    removed: bytes = b"",
) -> dict:
    return {
        "file": path,
        "operation": operation,
        "before_span": {"start": start, "end": end},
        "patch": {
            "inserted_base64": base64.b64encode(inserted).decode("ascii"),
            "inserted_sha256": _sha(inserted),
            "removed_base64": base64.b64encode(removed).decode("ascii"),
            "removed_sha256": _sha(removed),
        },
    }


def _history_stage(name: str, stdout: bytes = b"", stderr: bytes = b"") -> dict:
    return {
        "name": name,
        "tool": name,
        "status": "failed",
        "exit_code": 1,
        "stdout_base64": base64.b64encode(stdout).decode("ascii"),
        "stderr_base64": base64.b64encode(stderr).decode("ascii"),
    }


def _history_state(generations: list[dict], failures: dict[int, dict] | None = None) -> dict:
    """A kernel-state shape carrying only what the revision note reads."""
    events: list[dict] = []
    receipts: list[dict] = []
    sequence = 0
    for record in generations:
        sequence += 1
        events.append(
            {
                "kind": "worker_revision",
                "generation": record["generation"],
                "sequence": sequence,
                "evidence_sha256": _sha(f"revision-{record['generation']}".encode()),
            }
        )
        stage = (failures or {}).get(record["generation"])
        if stage is None:
            continue
        digest = _sha(f"receipt-{record['generation']}".encode())
        sequence += 1
        events.append(
            {
                "kind": "checker_failure",
                "generation": record["generation"],
                "sequence": sequence,
                "evidence_sha256": digest,
            }
        )
        receipts.append(
            {
                "receipt_sha256": digest,
                "receipt": {
                    "parsed_stdout": {
                        "check_id": f"check-{record['generation']}",
                        "ledger_generation": record["generation"],
                        "status": "failed",
                        "first_failing_stage": stage["name"],
                        "stages": [_history_stage("prepare"), stage],
                    }
                },
            }
        )
    return {
        "generation": generations[-1]["generation"] if generations else 0,
        "generation_history": generations,
        "event_history": events,
        "checker_receipts": receipts,
        "ledger": {"entries": []},
    }


def test_revision_history_reports_new_replaced_and_dropped_entries() -> None:
    first = _history_entry("unit_one.rs", "modify", 4, 9, b"let placeholder = 0;\n")
    shim = _history_entry("unit_two.rs", "add", 0, 0, b"pub fn shim() {}\n")
    replaced = _history_entry("unit_one.rs", "modify", 4, 9, b"let placeholder = 1;\n")
    removal = _history_entry("unit_three.rs", "delete", 2, 6, b"", b"stale\n")
    state = _history_state(
        [
            {"generation": 0, "ledger": {"entries": []}},
            {"generation": 1, "ledger": {"entries": [first]}},
            {"generation": 2, "ledger": {"entries": [first, shim]}},
            {"generation": 3, "ledger": {"entries": [replaced, removal]}},
        ],
        failures={
            2: _history_stage("translate", stdout=b"[Error] unresolved shape\n"),
        },
    )
    note = render_revision_history(state)
    assert note is not None
    assert note.startswith(REVISION_HISTORY_HEADER)
    # The empty baseline changed nothing, so it is not an item.
    assert "generation 0" not in note
    body = note[len(REVISION_HISTORY_HEADER):]
    order = [body.index(f"generation {value}") for value in (1, 2, 3)]
    assert order == sorted(order)  # oldest first, most recent last

    generation_one = body[body.index("generation 1"):body.index("generation 2")]
    assert "new unit_one.rs modify before_span 4..9" in generation_one
    assert "let placeholder = 0;" in generation_one
    assert "afterwards: no failure receipt for this generation." in generation_one

    generation_two = body[body.index("generation 2"):body.index("generation 3")]
    assert "new unit_two.rs add before_span 0..0" in generation_two
    assert "pub fn shim() {}" in generation_two
    # The entry carried forward unchanged is not repeated.
    assert "unit_one.rs" not in generation_two
    assert "afterwards: stage translate failed" in generation_two
    assert "[Error] unresolved shape" in generation_two

    generation_three = body[body.index("generation 3"):]
    assert "replaced unit_one.rs modify before_span 4..9" in generation_three
    assert "let placeholder = 1;" in generation_three
    assert "dropped unit_two.rs add before_span 0..0" in generation_three
    assert "new unit_three.rs delete before_span 2..6" in generation_three
    # Deletions and dropped rows insert nothing, so they show no inserted text.
    assert generation_three.count("inserted:") == 1


def test_revision_history_extracts_diagnostics_through_terminal_noise() -> None:
    progress = "\r".join(
        f"stage  [{'#' * step}{'-' * (3 - step)}] {step}/3" for step in range(4)
    )
    stdout = (
        "\x1b[?25l"
        + progress
        + "\x1b[?25h\n"
        + "[\x1b[91mError\x1b[39m] could not match the shapes\n"
        + "Source: 'unit_one.rs', lines 4:0-9:1\n"
        + "[\x1b[93mWarn\x1b[39m ] partial output written\n"
        + "Source: 'unit_one.rs', lines 40:0-41:1\n"
    ).encode("utf-8")
    stderr = (
        "error[E0001]: mismatched placeholder\n"
        "  --> unit_one.rs:4:1\n"
        "error: could not compile the unit\n"
        "note: a hint that is not a diagnostic opener\n"
        "errors are summarised above\n"
    ).encode("utf-8")
    stage = _history_stage("translate", stdout=stdout, stderr=stderr)

    lines = extract_stage_diagnostic(stage)
    assert lines == [
        "[Error] could not match the shapes",
        "Source: 'unit_one.rs', lines 4:0-9:1",
        "error[E0001]: mismatched placeholder",
        "--> unit_one.rs:4:1",
        "error: could not compile the unit",
    ]

    # Twelve lines and two thousand bytes bound the extract.
    many = _history_stage(
        "translate",
        stderr="".join(f"error: failure number {index}\n" for index in range(30)).encode(),
    )
    assert len(extract_stage_diagnostic(many)) == 12
    wide = _history_stage(
        "translate",
        stderr="".join(f"error: {'x' * 900}\n" for _ in range(6)).encode(),
    )
    bounded = extract_stage_diagnostic(wide)
    assert len(bounded) == 2
    assert sum(len(line.encode("utf-8")) + 1 for line in bounded) <= 2000
    assert extract_stage_diagnostic(_history_stage("translate")) == []


def test_revision_history_byte_cap_drops_the_oldest_generations_first() -> None:
    generations = [{"generation": 0, "ledger": {"entries": []}}]
    for index in range(1, 5):
        payload = f"marker-{index} ".encode() * 60
        generations.append(
            {
                "generation": index,
                "ledger": {"entries": [_history_entry("unit_one.rs", "modify", 0, 4, payload)]},
            }
        )
    state = _history_state(generations)

    full = render_revision_history(state)
    assert all(f"marker-{index}" in full for index in range(1, 5))

    capped = render_revision_history(state, max_bytes=len(full.encode("utf-8")) // 2)
    assert capped is not None
    assert len(capped.encode("utf-8")) <= len(full.encode("utf-8")) // 2
    assert capped.startswith(REVISION_HISTORY_HEADER)
    assert "marker-4" in capped and "marker-1" not in capped
    assert render_revision_history(state, max_bytes=len(full.encode("utf-8")) // 2) == capped

    windowed = render_revision_history(state, max_generations=2)
    assert "generation 3" in windowed and "generation 4" in windowed
    assert "generation 1" not in windowed and "generation 2" not in windowed

    short = render_revision_history(state, max_inserted_bytes=10)
    assert "… [truncated, 540 bytes]" in short


def test_revision_history_is_empty_without_accepted_generations() -> None:
    assert render_revision_history({}) is None
    assert render_revision_history({"generation_history": []}) is None
    # An accepted generation that changed no ledger row is not an item either.
    assert render_revision_history(
        _history_state([{"generation": 0, "ledger": {"entries": []}}])
    ) is None


def _prompt_dispatcher(attempt: Path, monkeypatch) -> tuple[ExistingLaneDispatcher, dict]:
    attempt.mkdir()
    dispatcher = ExistingLaneDispatcher(
        attempt, {"bindings": {"worker": {"binding_sha256": _sha(b"worker")}}},
    )
    seen: dict = {}

    class Response:
        ok = True
        error = ""

    def fake_execute(single, validate_artifact):
        seen["prompt"] = single.prompt
        raw = Path(single.state_dir) / "staging" / "req-1.raw.json"
        raw.parent.mkdir(parents=True, exist_ok=True)
        raw.write_text('{"ok": true}')
        return Response()

    monkeypatch.setattr("scripts.pv_phase0.execute_agent_request", fake_execute)
    monkeypatch.setattr(
        "scripts.pv_phase0.seed_burst_home",
        lambda root, name, persistent: (root / "burst-homes" / name),
    )
    monkeypatch.setattr(
        "scripts.pv_phase0.prepare_artifact_paths",
        lambda output, attempt_root, name: SimpleNamespace(
            raw=output / "staging" / "req-1.raw.json", done=output / "staging" / "req-1.done"
        ),
    )
    return dispatcher, seen


def test_worker_prompt_carries_the_note_after_the_contract_and_reviewers_do_not(
    tmp_path: Path, monkeypatch
) -> None:
    dispatcher, seen = _prompt_dispatcher(tmp_path / "attempt", monkeypatch)
    request = {"request_id": "req-1", "generation": 0, "request_sha256": _sha(b"req-1")}
    binding = {
        "provider": "codex", "model": "m", "effort": "low", "extra_args": [],
        "fallback_models": [], "lane_id": "worker", "binding_sha256": _sha(b"worker"),
    }
    fragment = "pv/source_adaptation/worker/05_role.md"
    common = dict(request=request, binding=binding, fragment=fragment, environment={})
    dispatcher._dispatch(  # noqa: SLF001
        role="source_adaptation_worker", persistent=True, **common
    )
    base = seen["prompt"]
    contract = canonical_json(request).decode("utf-8")
    assert contract in base

    note = "Revision history, derived by the framework from kernel state (synthetic)."
    dispatcher._dispatch(  # noqa: SLF001
        role="source_adaptation_worker", persistent=True, revision_history=note, **common
    )
    with_note = seen["prompt"]
    # The contract bytes are untouched and the note follows them.
    assert with_note.startswith(base)
    assert with_note.index(contract) < with_note.index(note)
    assert with_note == base + "\n\n" + note

    dispatcher._dispatch(  # noqa: SLF001
        role="source_adaptation_worker",
        persistent=True,
        revision_history=note,
        rejection="affected_targets names ['x']",
        **common,
    )
    both = seen["prompt"]
    assert both.index(note) < both.index("previous delivery for this request was rejected")

    dispatcher._dispatch(  # noqa: SLF001
        role="source_adaptation_reviewer", persistent=False, revision_history=note, **common
    )
    assert note not in seen["prompt"]


def _log_writing_dispatcher(attempt: Path, monkeypatch):
    """A dispatcher whose burst writes the live log set the way the agent
    wrapper does: fixed names per lane, with the transcript a symlink into the
    chat store that the next burst of the same lane truncates."""
    dispatcher, _seen = _prompt_dispatcher(attempt, monkeypatch)
    logs = attempt / "agent-output" / "logs"
    store = attempt / ".trellis" / "chats" / "phase0-worker"
    failures: dict[str, bool] = {"raise": False}

    def burst(single, validate_artifact):
        logs.mkdir(parents=True, exist_ok=True)
        store.mkdir(parents=True, exist_ok=True)
        transcript = store / "output.log"
        transcript.write_text(f"transcript of {single.request_id}\n", encoding="utf-8")
        link = logs / "phase0-worker-output.log"
        link.unlink(missing_ok=True)
        link.symlink_to(transcript)
        (logs / "phase0-worker.started").write_text(f"started {single.request_id}\n", encoding="utf-8")
        (logs / "phase0-worker.exit").write_text("0\n", encoding="utf-8")
        (logs / "phase0-seam-repair-0.exit").write_text("another lane\n", encoding="utf-8")
        if failures["raise"]:
            raise RuntimeError("transport died mid-burst")
        raw = Path(single.state_dir) / "staging" / "req-1.raw.json"
        raw.parent.mkdir(parents=True, exist_ok=True)
        raw.write_text('{"ok": true}')
        return SimpleNamespace(ok=True, error="")

    monkeypatch.setattr("scripts.pv_phase0.execute_agent_request", burst)
    return dispatcher, logs, failures


def _dispatch_worker(dispatcher, request_id: str) -> None:
    dispatcher._dispatch(  # noqa: SLF001
        request={"request_id": request_id, "generation": 0, "request_sha256": _sha(request_id.encode())},
        binding={
            "provider": "codex", "model": "m", "effort": "low", "extra_args": [],
            "fallback_models": [], "lane_id": "worker", "binding_sha256": _sha(b"worker"),
        },
        fragment="pv/source_adaptation/worker/05_role.md",
        role="source_adaptation_worker",
        environment={},
        persistent=True,
    )


def test_every_burst_keeps_its_own_log_set_and_the_live_markers_stay_put(
    tmp_path: Path, monkeypatch
) -> None:
    attempt = tmp_path / "attempt"
    dispatcher, logs, _failures = _log_writing_dispatcher(attempt, monkeypatch)

    _dispatch_worker(dispatcher, "worker-0")
    _dispatch_worker(dispatcher, "worker-1")

    sets = sorted(path.name for path in (logs / "history").iterdir())
    assert sets == ["000001-worker-0", "000002-worker-1"]
    # The refused burst's transcript survives the burst that overwrote the
    # file it was linked to, and is a real file, not a link to the live one.
    first = logs / "history" / "000001-worker-0" / "phase0-worker-output.log"
    assert not first.is_symlink()
    assert first.read_text(encoding="utf-8") == "transcript of worker-0\n"
    assert (logs / "history" / "000001-worker-0" / "phase0-worker.started").read_text(
        encoding="utf-8"
    ) == "started worker-0\n"
    assert (logs / "history" / "000002-worker-1" / "phase0-worker-output.log").read_text(
        encoding="utf-8"
    ) == "transcript of worker-1\n"
    # Only this lane's set is retained by this lane's burst.
    assert not (logs / "history" / "000001-worker-0" / "phase0-seam-repair-0.exit").exists()
    # The live burst still publishes its markers and transcript at the
    # unchanged, predictable paths a monitor watches.
    assert (logs / "phase0-worker.started").read_text(encoding="utf-8") == "started worker-1\n"
    assert (logs / "phase0-worker.exit").read_text(encoding="utf-8") == "0\n"
    assert (logs / "phase0-worker-output.log").read_text(encoding="utf-8") == "transcript of worker-1\n"


def test_refused_draft_is_retained_with_its_reason_in_the_burst_log_set(tmp_path: Path) -> None:
    delivered = {
        "schema": "trellis-phase0-worker-draft/v1",
        "request_id": "worker-0",
        "worker_thread_id": "thread-1",
        "entries": [],
        "rewrite_pattern": "unknown field",
    }

    def refused(_request, _environment, **_extra):
        return dict(delivered)

    driver, _candidate, _authority, _state = _driver_fixture(tmp_path, refused)
    history = driver.attempt / "agent-output" / "logs" / "history"

    with pytest.raises(Phase0DraftRejected, match="missing or unknown fields"):
        driver._worker_burst({"request_id": "worker-0"})  # noqa: SLF001

    sets = sorted(path.name for path in history.iterdir())
    assert sets == ["000001-worker-0"]
    retained = json.loads((history / "000001-worker-0" / "refused-draft.json").read_text(encoding="utf-8"))
    assert retained["request_id"] == "worker-0"
    assert "missing or unknown fields" in retained["reason"]
    assert retained["draft"] == delivered

    # A second refusal of the same request keeps its own set rather than
    # overwriting the first.
    with pytest.raises(Phase0DraftRejected):
        driver._worker_burst({"request_id": "worker-0"})  # noqa: SLF001
    assert sorted(path.name for path in history.iterdir()) == [
        "000001-worker-0", "000002-worker-0",
    ]


def test_refused_draft_joins_the_log_set_the_dispatch_already_retained(
    tmp_path: Path, monkeypatch
) -> None:
    attempt = tmp_path / "driver-attempt"
    dispatcher, logs, _failures = _log_writing_dispatcher(attempt, monkeypatch)

    def refused(request, _environment, **_extra):
        _dispatch_worker(dispatcher, request["request_id"])
        return {"malformed": True}

    driver, _candidate, _authority, _state = _driver_fixture(tmp_path, refused)
    monkeypatch.setattr(driver, "agent_output", dispatcher.output)

    with pytest.raises(Phase0DraftRejected):
        driver._worker_burst({"request_id": "worker-0"})  # noqa: SLF001

    # One set for the burst, holding both its transcript and the refused bytes.
    assert sorted(path.name for path in (logs / "history").iterdir()) == ["000001-worker-0"]
    retained = logs / "history" / "000001-worker-0"
    assert (retained / "phase0-worker-output.log").read_text(encoding="utf-8") == "transcript of worker-0\n"
    assert json.loads((retained / "refused-draft.json").read_text(encoding="utf-8"))["draft"] == {
        "malformed": True
    }


def test_burst_log_retention_is_bounded_and_survives_a_transport_failure(
    tmp_path: Path, monkeypatch
) -> None:
    attempt = tmp_path / "attempt"
    dispatcher, logs, failures = _log_writing_dispatcher(attempt, monkeypatch)
    monkeypatch.setattr("scripts.pv_phase0.PHASE0_LOG_HISTORY_SETS", 2)

    _dispatch_worker(dispatcher, "worker-0")
    failures["raise"] = True
    with pytest.raises(RuntimeError, match="transport died mid-burst"):
        _dispatch_worker(dispatcher, "worker-1")
    failures["raise"] = False
    _dispatch_worker(dispatcher, "worker-2")

    # The failed burst is retained too, and the bound drops the oldest set.
    sets = sorted(path.name for path in (logs / "history").iterdir())
    assert sets == ["000002-worker-1", "000003-worker-2"]
    assert (logs / "history" / "000002-worker-1" / "phase0-worker-output.log").read_text(
        encoding="utf-8"
    ) == "transcript of worker-1\n"


def _retaining_broker(tmp_path: Path) -> Phase0CheckerBroker:
    template = tmp_path / "template.json"
    template.write_bytes(canonical_json({"goal_path": str(tmp_path / "GOAL.md")}))
    return Phase0CheckerBroker(
        kernel_cli=tmp_path / "kernel-cli",
        checker_runner=tmp_path / "checker.py",
        working_directory=tmp_path / "authority",
        request_template=template,
        authority_root=tmp_path / "authority",
        state_path=tmp_path / "authority/state.json",
        candidate_path=tmp_path / "candidate",
        interactive_candidate_root=tmp_path / "attempt/agent-output/logs/interactive-candidates",
    )


def _interactive_check(tmp_path: Path, ordinal: int) -> dict:
    frozen = tmp_path / f"frozen-{ordinal}"
    frozen.mkdir()
    (frozen / "lib.rs").write_text(f"pub fn f() {{ {ordinal} }}\n", encoding="utf-8")
    return {
        "receipt": {"receipt_sha256": _sha(f"receipt-{ordinal}".encode())},
        "output": {
            "check_id": f"phase0-0-candidate-{ordinal}",
            "status": "failed",
            "first_failing_stage": "cargo_check",
            "candidate_tree_sha256": _sha(f"tree-{ordinal}".encode()),
        },
        "frozen_tree": str(frozen),
    }


def test_interactive_candidate_trees_are_retained_and_bounded(
    tmp_path: Path, monkeypatch
) -> None:
    broker = _retaining_broker(tmp_path)
    monkeypatch.setattr("scripts.pv_phase0.PHASE0_INTERACTIVE_CANDIDATE_SETS", 2)
    root = tmp_path / "attempt/agent-output/logs/interactive-candidates"

    for ordinal in (1, 2, 3):
        checked = _interactive_check(tmp_path, ordinal)
        assert broker.retain_interactive_candidate(checked) is not None
        broker.cleanup(checked)
        # The check's own frozen tree is gone; only the retained copy survives.
        assert not Path(checked["frozen_tree"]).exists()

    sets = sorted(path.name for path in root.iterdir())
    assert sets == [
        "000002-phase0-0-candidate-2",
        "000003-phase0-0-candidate-3",
    ]
    kept = root / "000003-phase0-0-candidate-3"
    assert (kept / "candidate/lib.rs").read_text(encoding="utf-8") == "pub fn f() { 3 }\n"
    record = json.loads((kept / "check.json").read_bytes())
    assert record["check_id"] == "phase0-0-candidate-3"
    assert record["status"] == "failed"
    assert record["first_failing_stage"] == "cargo_check"
    assert record["receipt_sha256"] == _sha(b"receipt-3")


def test_interactive_candidate_retention_is_best_effort(tmp_path: Path) -> None:
    broker = _retaining_broker(tmp_path)
    checked = _interactive_check(tmp_path, 1)
    shutil.rmtree(checked["frozen_tree"])
    assert broker.retain_interactive_candidate(checked) is None

    without_root = _retaining_broker(tmp_path)
    without_root.interactive_candidates = None
    assert without_root.retain_interactive_candidate(_interactive_check(tmp_path, 2)) is None


def test_driver_hands_the_worker_a_note_derived_from_kernel_state(tmp_path: Path) -> None:
    calls: list[dict] = []

    def worker(_request, _environment, rejection=None, revision_history=None):
        calls.append({"rejection": rejection, "revision_history": revision_history})
        return {"malformed": True}

    driver, _candidate, _authority, state_path = _driver_fixture(tmp_path, worker)
    with pytest.raises(Phase0DraftRejected):
        driver._worker_burst({"request_id": "worker-0"})  # noqa: SLF001
    # An empty history adds nothing to the dispatch.
    assert calls[0]["revision_history"] is None

    state = _history_state(
        [
            {"generation": 0, "ledger": {"entries": []}},
            {
                "generation": 1,
                "ledger": {
                    "entries": [_history_entry("unit_one.rs", "add", 0, 0, b"pub fn shim() {}\n")]
                },
            },
        ],
        failures={1: _history_stage("translate", stdout=b"[Error] unresolved shape\n")},
    )
    state["state_sha256"] = _sha(b"state-1")
    state_path.write_bytes(canonical_json(state))
    with pytest.raises(Phase0DraftRejected):
        driver._worker_burst({"request_id": "worker-1"})  # noqa: SLF001
    handed = calls[1]["revision_history"]
    assert handed == render_revision_history(state)
    assert "new unit_one.rs add before_span 0..0" in handed
    assert "afterwards: stage translate failed" in handed


def test_revision_history_includes_the_current_generation_and_reason_fallback() -> None:
    """Kernel state retires a generation into `generation_history` only when the
    next one is accepted; the current ledger is `state["ledger"]` and must be
    the newest item, paired with the failure that follows it."""
    baseline = {"generation": 0, "ledger": {"entries": []}}
    first_entry = _history_entry("unit_one.rs", "add", 0, 0, inserted=b"[package]\n")
    state = _history_state([baseline], failures={})
    state["generation"] = 1
    state["ledger"] = {"entries": [first_entry]}
    digest = _sha(b"receipt-current-1")
    state["event_history"].append(
        {"kind": "checker_failure", "generation": 1, "sequence": 99, "evidence_sha256": digest}
    )
    state["checker_receipts"].append(
        {
            "receipt_sha256": digest,
            "receipt": {
                "parsed_stdout": {
                    "check_id": "check-current-1",
                    "ledger_generation": 1,
                    "status": "failed",
                    "first_failing_stage": "standalone_package",
                    "reason": "standalone lock file is unavailable: the candidate must carry it",
                    "stages": [_history_stage("standalone_package")],
                }
            },
        }
    )
    note = render_revision_history(state)
    assert note is not None
    assert "generation 1" in note
    assert "new unit_one.rs add before_span 0..0" in note
    assert "afterwards: stage standalone_package failed" in note
    assert "standalone lock file is unavailable" in note
    # Once the kernel retires generation 1 into the history the same item is
    # rendered exactly once.
    state["generation_history"].append({"generation": 1, "ledger": {"entries": [first_entry]}})
    assert render_revision_history(state) == note


def test_revision_history_caps_diagnostics_not_location_lines() -> None:
    stderr = "".join(
        f"error[E{index:04d}]: failure number {index}\n  --> unit_one.rs:{index}:1\n"
        for index in range(20)
    ).encode()
    lines = extract_stage_diagnostic(_history_stage("build", stderr=stderr))
    assert sum(1 for line in lines if line.startswith("error")) == 12
    assert sum(1 for line in lines if line.startswith("-->")) == 12
    stdout = "".join(
        f"[Error] failure number {index}\nSource: 'unit_two.rs', lines {index}:0-{index}:1\n"
        for index in range(20)
    ).encode()
    lines = extract_stage_diagnostic(_history_stage("translate", stdout=stdout))
    assert sum(1 for line in lines if line.startswith("[Error]")) == 12
    assert len(lines) == 24


def test_revision_history_names_a_relocated_diagnostic() -> None:
    """An edit that moves an error to another line leaves the diagnostic
    texts unchanged; the note says so instead of presenting it as progress."""
    first = _history_entry("unit_one.rs", "modify", 10, 20, inserted=b"a", removed=b"b")
    second = _history_entry("unit_one.rs", "modify", 40, 50, inserted=b"c", removed=b"d")
    stage_one = _history_stage(
        "translate", stdout=b"[Error] Found type error\nSource: 'unit_one.rs', lines 3:1-3:9\n"
    )
    stage_two = _history_stage(
        "translate", stdout=b"[Error] Found type error\nSource: 'unit_one.rs', lines 7:1-7:9\n"
    )
    stage_three = _history_stage("translate", stdout=b"[Error] Unreachable\nSource: 'unit_one.rs', lines 9:1-9:2\n")
    third = _history_entry("unit_two.rs", "modify", 1, 2, inserted=b"e", removed=b"f")
    state = _history_state(
        [
            {"generation": 0, "ledger": {"entries": []}},
            {"generation": 1, "ledger": {"entries": [first]}},
            {"generation": 2, "ledger": {"entries": [first, second]}},
            {"generation": 3, "ledger": {"entries": [first, second, third]}},
        ],
        failures={1: stage_one, 2: stage_two, 3: stage_three},
    )
    note = render_revision_history(state)
    assert note is not None
    assert "the diagnostic texts are the same as after generation 1; only locations differ" in note
    assert note.count("only locations differ") == 1
