"""Grunt attempt runner seams that do not need a Lean toolchain.

``tests/test_sidecar_e2e.py`` drives ``run_grunt_attempt`` for real, but
that whole module is skipped without a lake toolchain and a built kernel
binary — so the runner's own reporting had no coverage anywhere a plain
`pytest` run would see it. These tests stub the workspace and compile
loop (both already exercised on their own) and the codex generator,
leaving exactly the runner's outcome/report surface under test.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Dict, List

import pytest

from trellis.sidecar import attempt as attempt_mod
from trellis.sidecar.config import SidecarConfig
from trellis.sidecar.driver import AttemptResult

NODE_FILE = (
    "import Tablet.Preamble\n\n-- [TABLET NODE: Rung]\n"
    "theorem Rung : True := by\n-- BODY\n  sorry\n"
)


class _FakeLoop:
    """The compile-loop surface ``run_grunt_attempt`` touches."""

    def __init__(self, *_args: Any, **_kwargs: Any) -> None:
        self.restored: List[str] = []
        self.shutdowns = 0

    def giant_reason(self, _node: str):
        return None

    def open_node(self, _node: str, _content: str) -> None:
        return None

    def restore(self, node: str, _content: str) -> None:
        self.restored.append(node)

    def shutdown(self) -> None:
        self.shutdowns += 1


def _stub_runner(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, result: AttemptResult
) -> _FakeLoop:
    """Point ``run_grunt_attempt`` at a paper workspace and a canned
    generator result; returns the loop it will use."""
    from trellis.sidecar import compile_loop as compile_loop_mod
    from trellis.sidecar import workspace as workspace_mod

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet" / "Rung.lean").write_text(NODE_FILE, encoding="utf-8")

    loop = _FakeLoop()
    monkeypatch.setattr(
        workspace_mod,
        "refresh_to_snapshot",
        lambda *_a, **_k: workspace_mod.RefreshResult(ok=True, reason="", head="sha"),
    )
    monkeypatch.setattr(workspace_mod, "workspace_repo_path", lambda *_a, **_k: repo)
    monkeypatch.setattr(workspace_mod, "purge_stale_oleans_for", lambda *_a: [])
    monkeypatch.setattr(compile_loop_mod, "SidecarCompileLoop", lambda *_a, **_k: loop)
    monkeypatch.setattr(
        compile_loop_mod, "make_compile_callback", lambda *_a, **_k: (lambda _b: None)
    )
    monkeypatch.setattr(attempt_mod, "read_api_key", lambda _cfg: "sk-test")
    monkeypatch.setattr(attempt_mod, "run_attempt_codex", lambda **_kw: result)
    return loop


def _run(tmp_path: Path) -> Dict[str, Any]:
    return attempt_mod.run_grunt_attempt(
        tmp_path / "runtime",
        grunt=0,
        node="Rung",
        entry_seq=7,
        snapshot_sha="sha",
        node_file_sha="",
        statement_prefix_sha="sp",
        attempt_id="sc-test",
        config=SidecarConfig(enabled=True, attempt_wall_seconds=5400.0),
    )


def test_prior_kernel_rejection_is_available_to_the_next_grunt(tmp_path: Path) -> None:
    feedback = tmp_path / "runtime" / "sidecar" / "feedback"
    feedback.mkdir(parents=True)
    (feedback / "feedback-old.json").write_text(
        """{
          "feedback_id": "old",
          "attempt_id": "sc-old",
          "node": "Rung",
          "status": "rejected",
          "detail": "closure_probe (axiom violation)",
          "cycle": 175
        }""",
        encoding="utf-8",
    )
    prompt_feedback = attempt_mod._prior_kernel_feedback(
        tmp_path / "runtime", "Rung", "sc-new"
    )
    assert "PRIOR ATTEMPTS" in prompt_feedback
    assert "closure_probe (axiom violation)" in prompt_feedback


def test_failed_attempt_log_line_names_the_cause(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """The WHY of a non-success rides the operator-visible line. Without
    it an incident of this class is diagnosable only by reconstructing
    the ledger after the fact, which is exactly what the transport
    misclassification cost: the log said "attempt error" and nothing
    more."""
    loop = _stub_runner(
        monkeypatch,
        tmp_path,
        AttemptResult(
            status="error",
            detail="model request failed: ReadTimeout",
            iterations=134,
            wall_secs=5430.1,
            compactions=2,
            transport_retries=1,
        ),
    )
    outcome = _run(tmp_path)
    assert outcome["status"] == "error"
    line = capsys.readouterr().out.strip()
    assert "(model request failed: ReadTimeout)" in line, (
        "the cause must be ON the log line, not only in the ledger row"
    )
    assert "attempt error (" in line
    assert "134 iterations" in line and "1 transport retries" in line
    assert loop.restored == ["Rung"], "a failed attempt leaves the node as it was"
    assert loop.shutdowns == 1


def test_successful_attempt_log_line_carries_no_why(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """The parenthetical is the failure's reason, so success — whose
    detail is empty — reads clean."""
    _stub_runner(
        monkeypatch,
        tmp_path,
        AttemptResult(status="failed", detail="", iterations=3, wall_secs=12.0),
    )
    _run(tmp_path)
    line = capsys.readouterr().out.strip().splitlines()[-1]
    assert line.startswith("sidecar[sc-test]: attempt failed after 3 iterations")
    assert "(" not in line.split("info tools")[0]
