"""Sidecar compile loop + prevalidation over a REAL mathlib-free
micro-workspace (plan commit 10; test plan §6.3).

Self-built in-test fixture: a minimal FILESPEC-shaped lake project (no
mathlib), toolchain ``v4.30.0-rc1`` from ``~/.elan``, a Preamble and a
``-- BODY``-marked node proving Nat-arithmetic trivia, created under
the pytest basetemp (``~/.cache/trellis-pytest`` — never ``/tmp``).
Real LSP engine unsandboxed via ``INCREMENTAL_CHECK_ALLOW_UNSANDBOXED=1``
(documented offline escape hatch); the compile callback is driven
directly with a failing body then the correct one. Exercises splice →
didChange → diagnostics → confirm-lake-build → probes → spooled
``success`` record, with zero API tokens.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path

import pytest

from trellis.sidecar.compile_loop import (
    SIDECAR_INJECTED_OPTIONS,
    SidecarCompileLoop,
    _inject_sidecar_options,
    make_compile_callback,
    make_goal_tool_handler,
)
from trellis.sidecar.config import SidecarConfig
from trellis.sidecar.driver import AttemptResult, split_body_marker
from trellis.sidecar.prevalidate import prevalidate_success
from trellis.sidecar.spool import ensure_spool_dirs, publish_attempt, spool_dirs

TOOLCHAIN = "leanprover/lean4:v4.30.0-rc1"

NODE_FILE = (
    "import Tablet.Preamble\n\n-- [TABLET NODE: Rung]\n"
    "theorem Rung : 1 + 1 = 2 := by\n-- BODY\n  sorry\n"
)


def _lake_available() -> bool:
    if shutil.which("lake") is None:
        return False
    toolchain_dir = Path.home() / ".elan" / "toolchains" / "leanprover--lean4---v4.30.0-rc1"
    return toolchain_dir.exists()


pytestmark = pytest.mark.skipif(
    not _lake_available(), reason="lake / v4.30.0-rc1 toolchain unavailable"
)


def _git(repo: Path, *args: str) -> None:
    proc = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        env={
            **os.environ,
            "GIT_AUTHOR_NAME": "t",
            "GIT_AUTHOR_EMAIL": "t@t",
            "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@t",
        },
    )
    assert proc.returncode == 0, proc.stderr


@pytest.fixture(scope="module")
def micro_workspace(tmp_path_factory: pytest.TempPathFactory) -> Path:
    repo = tmp_path_factory.mktemp("sidecar-micro") / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "lean-toolchain").write_text(TOOLCHAIN + "\n")
    (repo / "lakefile.lean").write_text(
        "import Lake\nopen Lake DSL\n\npackage tablet\n\n"
        "lean_lib Tablet where\n  globs := #[.submodules `Tablet]\n"
    )
    (repo / "Tablet" / "Preamble.lean").write_text(
        "-- Tablet preamble (mathlib-free micro-workspace)\n"
    )
    (repo / "Tablet" / "Rung.lean").write_text(NODE_FILE)
    (repo / ".gitignore").write_text(".lake/\n*.olean\n")
    # Warm build FIRST (the open node builds with a sorry warning,
    # exit 0), so the lake-manifest.json it generates is committed like
    # a real tablet repo's tracked manifest.
    proc = subprocess.run(
        ["lake", "build", "Tablet.Rung"],
        cwd=str(repo),
        capture_output=True,
        text=True,
        timeout=600,
    )
    assert proc.returncode == 0, proc.stdout + proc.stderr
    _git(repo, "init", "-q")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "seed micro-workspace")
    return repo


def _cfg() -> SidecarConfig:
    return SidecarConfig(
        enabled=True,
        sandbox_role="",  # offline harness on a throwaway copy
        lean_threads=2,
        max_iterations=6,
        attempt_wall_seconds=600.0,
        attempt_tokens=500_000,
    )


def test_inject_sidecar_options_shape() -> None:
    injected, insert_line, count = _inject_sidecar_options(NODE_FILE)
    assert count == 2
    lines = injected.splitlines()
    assert lines[insert_line] == SIDECAR_INJECTED_OPTIONS[0]
    assert "maxHeartbeats 400000" in injected
    assert "maxHeartbeats 0" not in injected, "the broker ceiling must not leak in"


def test_giant_node_skip(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet" / "Big.lean").write_text(NODE_FILE + "-- pad\n" * 50)
    loop = SidecarCompileLoop(
        repo, SidecarConfig(enabled=True, giant_node_max_lines=10, sandbox_role="")
    )
    reason = loop.giant_reason("Big")
    assert reason is not None and "lines" in reason


def test_end_to_end_fail_then_success_over_real_lean(
    micro_workspace: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("INCREMENTAL_CHECK_ALLOW_UNSANDBOXED", "1")
    repo = micro_workspace
    config = _cfg()
    loop = SidecarCompileLoop(repo, config)
    try:
        opened = loop.open_node("Rung", NODE_FILE)
        # Failure-open design: even if the warm open is ambiguous the
        # lake fallback still carries the attempt.
        compile_body = make_compile_callback(loop, "Rung", NODE_FILE)
        # The generator is out of scope here (the codex arm owns it); what
        # this rig exists to exercise is the callback itself — a bad body
        # comes back red with compiler text, the good one comes back green.
        bad = compile_body("  exact nonexistent_lemma_xyz\n")
        assert not bad.ok
        assert bad.log.strip(), "a failing compile must carry compiler output"
        good = compile_body("  rfl\n")
        assert good.ok, good.log
        result = AttemptResult(
            status="success",
            proof_body="  rfl\n",
            iterations=2,
            prompt_tokens=100,
            completion_tokens=20,
        )
        assert opened in (True, False)

        # The confirming build left the workspace file spliced; the
        # prevalidation audits it and stamps fingerprints.
        prefix, _ = split_body_marker(NODE_FILE)
        pre = prevalidate_success(
            repo=repo,
            node="Rung",
            config=config,
            pre_image=NODE_FILE,
            proof_body=result.proof_body,
        )
        assert pre.ok, pre.reasons
        assert pre.axioms == [], f"rfl proof must be axiom-free: {pre.axioms}"
        # The probe RUNS now. This asserted `skipped` while
        # `run_local_closure_probe` looked for the Lean script at
        # `<repo>/scripts/`, which exists in no tablet and no grunt
        # workspace — so the probe was permanently skipped and the
        # `#print axioms` fallback was the only axiom check that ever ran
        # in production. The script lives in the trellis source tree, and
        # is resolved from there now. `pre.axioms == []` above is
        # therefore the LOCAL closure's `kernel_axioms`, which is the set
        # the kernel's gate 8 installs from.
        assert pre.local_closure_status == "ok", pre.reasons
        assert pre.fingerprints["statement_prefix_sha256"]
        assert pre.fingerprints["source_closure_hash"] == "", (
            "micro-workspace has no check.py; the closure hash fails closed"
        )

        # Spooled success record (the §6.3 chain end).
        from trellis.sidecar.driver import build_attempt_record

        record = build_attempt_record(
            config=config,
            attempt_id="sc-micro-1",
            node="Rung",
            entry_seq=1,
            snapshot_sha="micro",
            candidate={},
            workspace_fingerprints=pre.fingerprints,
            result=result,
            daemon_validation=pre.daemon_validation(),
        )
        dirs = spool_dirs(tmp_path)
        ensure_spool_dirs(dirs)
        published = publish_attempt(dirs, record)
        body = json.loads(published.read_text())
        assert body["status"] == "success"
        assert body["artifact"]["proof_body"] == "  rfl\n"
    finally:
        loop.restore("Rung", NODE_FILE)
        loop.shutdown()


def test_prevalidate_catches_out_of_envelope_edit(
    micro_workspace: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The only-BODY audit is an independent belt: a (simulated) driver
    bug that touched a second file must fail prevalidation."""
    repo = micro_workspace
    stray = repo / "Tablet" / "Preamble.lean"
    original = stray.read_text()
    node_path = repo / "Tablet" / "Rung.lean"
    node_original = node_path.read_text()
    try:
        prefix, _ = split_body_marker(NODE_FILE)
        node_path.write_text(prefix + "  rfl\n")
        stray.write_text(original + "-- stray edit\n")
        pre = prevalidate_success(
            repo=repo,
            node="Rung",
            config=_cfg(),
            pre_image=NODE_FILE,
            proof_body="  rfl\n",
        )
        assert not pre.ok
        assert any("only_body" in reason for reason in pre.reasons)
    finally:
        stray.write_text(original)
        node_path.write_text(node_original)


# ---------------------------------------------------------------------------
# v3 get_goals (advisory goal state over the warm server) — unit-level,
# no real Lean: a fake server exercises position mapping + response
# parsing + graceful degradation.
# ---------------------------------------------------------------------------


class _FakeGoalServer:
    def __init__(self, result, *, alive=True, rid=42):
        self._result = result
        self._alive = alive
        self._rid = rid
        self.last_position = None

    def alive(self) -> bool:
        return self._alive

    def request(self, method, params):
        assert method == "$/lean/plainGoal"
        self.last_position = params["position"]
        return self._rid

    def drain(self, timeout):
        return {"id": self._rid, "result": self._result}


def _goals_loop(tmp_path: Path, server, *, body="  sorry\n", prefix="import X\n-- BODY\n"):
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True, exist_ok=True)
    loop = SidecarCompileLoop(repo, SidecarConfig(enabled=True))
    loop._server = server
    loop._open_node = "Rung"
    loop._last_prefix = prefix
    loop._last_body = body
    # inject_geometry: 2 option lines inserted at line 1 (inside prefix).
    loop._inject_geometry = (1, 2)
    return loop


def test_get_goals_renders_goal_state(tmp_path: Path) -> None:
    server = _FakeGoalServer({"rendered": "⊢ True"})
    loop = _goals_loop(tmp_path, server)
    handler = make_goal_tool_handler(loop, "Rung")
    out = handler({"line": 1})
    assert "⊢ True" in out
    # prefix has 2 newlines + 2 injected lines + (line 1 - 1) = doc line 4.
    assert server.last_position["line"] == 4


def test_get_goals_defaults_to_sorry_line(tmp_path: Path) -> None:
    server = _FakeGoalServer({"goals": ["⊢ False"]})
    loop = _goals_loop(tmp_path, server, body="  intro h\n  sorry\n")
    handler = make_goal_tool_handler(loop, "Rung")
    out = handler({})  # no line -> first line containing `sorry` (body line 2)
    assert "⊢ False" in out
    # body line 2 -> prefix(2) + injected(2) + (2-1) = doc line 5.
    assert server.last_position["line"] == 5


def test_get_goals_degrades_without_a_server(tmp_path: Path) -> None:
    loop = _goals_loop(tmp_path, None)
    loop._server = None
    out = make_goal_tool_handler(loop, "Rung")({"line": 1})
    assert "goal state unavailable" in out


def test_get_goals_reports_empty_goal_state(tmp_path: Path) -> None:
    server = _FakeGoalServer(None)  # no result -> no goal state message
    loop = _goals_loop(tmp_path, server)
    out = make_goal_tool_handler(loop, "Rung")({"line": 1})
    assert "no goal state" in out
