"""Joint sidecar E2E rig (plan commit 12, §6.4; amendment A6).

The smallest REAL end-to-end: a mathlib-free micro-workspace promoted
to a git repo in the production two-repo layout
(``r/`` worker + ``r/.trellis/supervisor/repo`` authoritative +
``r/.trellis/runtime/rt`` runtime), with

* a **REAL checker server** (``trellis.checker.server.CheckerServer``)
  over the workspace, its socket handed to the kernel ingest via
  ``TRELLIS_CHECKER_SOCKET`` — so apply gates 7–8 (compile probe,
  axioms/local-closure) run for REAL, and the test asserts the gate
  traffic hit the socket (amendment A6);
* the **real per-repo ``check.py`` dispatch** (a shim onto
  ``trellis.atomic_actions.cli`` — the same engine the generated
  production script wraps), so the kernel's
  ``run_repo_command_json`` path is exercised verbatim;
* the **daemon pipeline with a mocked model** (scripted good body run
  → one ``pending/`` record; scripted bad run → none);
* one **kernel ``Run`` boundary** that claims, gates, applies, logs
  the ``sidecar_closure`` event, records provenance, mirrors the
  worker repo, and exports post-apply ``candidates.json``;
* a **doctored-stale record** rejected as ``stale_content`` with the
  worktree untouched.

Zero API tokens; no live-run involvement; no ``/tmp`` scratch (the rig
lives under ``~/.cache`` — short paths for the AF_UNIX 108-byte cap).
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import signal
import subprocess
import tempfile
import threading
import time
from pathlib import Path
from typing import Any, Dict, List

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]

KERNEL_BIN_CANDIDATES = [
    REPO_ROOT / "kernel" / "target-worktree" / "debug" / "trellis_runtime_cli",
    REPO_ROOT / "kernel" / "target" / "debug" / "trellis_runtime_cli",
    REPO_ROOT / "kernel" / "target" / "release" / "trellis_runtime_cli",
]

TOOLCHAIN = "leanprover/lean4:v4.30.0-rc1"

NODE_FILE = (
    "import Tablet.Preamble\n\n-- [TABLET NODE: Rung]\n"
    "theorem Rung : 1 + 1 = 2 := by\n-- BODY\n  sorry\n"
)

NODE_TEX = (
    "\\begin{theorem}\\label{thm:rung}\n1 + 1 = 2.\n\\end{theorem}\n"
    "\\begin{proof}\nImmediate.\n\\end{proof}\n"
)

LIFT_FILE = (
    "import Tablet.Preamble\n\n-- [TABLET NODE: Lift]\n"
    "theorem Lift : 2 = 2 := by\n-- BODY\n  sorry\n"
)

LIFT_TEX = (
    "\\begin{theorem}\\label{thm:lift}\n2 = 2.\n\\end{theorem}\n"
    "\\begin{proof}\nImmediate.\n\\end{proof}\n"
)


def _kernel_bin() -> Path | None:
    """The kernel CLI this suite drives, or None when there is none.

    A ``TRELLIS_KERNEL_BIN`` that does not exist is a HARD ERROR naming
    the path, never a fallthrough to ``KERNEL_BIN_CANDIDATES``: the
    operator who set that variable was pointing the suite at a
    particular build, and silently testing a DIFFERENT binary — very
    plausibly a months-old one still sitting in ``target/release`` —
    produces a green run that says nothing about the build under
    test."""
    override = os.environ.get("TRELLIS_KERNEL_BIN", "").strip()
    if override:
        path = Path(override)
        if not path.exists():
            raise RuntimeError(
                f"TRELLIS_KERNEL_BIN points at {path}, which does not exist. "
                "Build it, or unset the variable to use the default "
                f"candidates ({', '.join(str(c) for c in KERNEL_BIN_CANDIDATES)})."
            )
        return path
    for candidate in KERNEL_BIN_CANDIDATES:
        if candidate.exists():
            return candidate
    return None


def _lake_available() -> bool:
    if shutil.which("lake") is None:
        return False
    return (
        Path.home() / ".elan" / "toolchains" / "leanprover--lean4---v4.30.0-rc1"
    ).exists()


pytestmark = pytest.mark.skipif(
    not (_lake_available() and _kernel_bin() is not None),
    reason=(
        "lake toolchain or built trellis_runtime_cli unavailable — build the "
        "kernel with `cargo build --bin trellis_runtime_cli --manifest-path "
        "kernel/Cargo.toml` (the DEBUG profile is what this suite and the "
        "runtime resolve), and install the Lean toolchain "
        f"{TOOLCHAIN} with scripts/install_lean_toolchain.sh"
    ),
)


def _sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _git(repo: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        env={
            **os.environ,
            "GIT_AUTHOR_NAME": "rig",
            "GIT_AUTHOR_EMAIL": "rig@t",
            "GIT_COMMITTER_NAME": "rig",
            "GIT_COMMITTER_EMAIL": "rig@t",
        },
    )
    assert proc.returncode == 0, f"git {args}: {proc.stderr}"
    return proc.stdout.strip()


CHECK_PY_SHIM = f"""#!/usr/bin/env python3
# Rig check.py: the REAL dispatch engine (trellis.atomic_actions.cli),
# exactly what the generated production script wraps. Ops route to the
# live checker server through TRELLIS_CHECKER_SOCKET. The
# sync-supervisor-workspace op (a wrapper-level extra in the generated
# script) delegates to the real trellis.supervisor_workspace.
import json as _json
import sys
from pathlib import Path
sys.path.insert(0, {str(REPO_ROOT)!r})
if sys.argv[1:2] == ["sync-supervisor-workspace"]:
    from trellis.supervisor_workspace import sync_supervisor_workspace
    supervisor_repo = Path(__file__).resolve().parents[2]
    worker_repo = supervisor_repo.parents[2]
    print(_json.dumps(sync_supervisor_workspace(worker_repo), default=str))
    sys.exit(0)
from trellis.atomic_actions.cli import main
sys.exit(main(sys.argv[1:]))
"""


def _seed_tablet(repo: Path) -> None:
    (repo / "Tablet").mkdir(parents=True, exist_ok=True)
    (repo / "lean-toolchain").write_text(TOOLCHAIN + "\n")
    (repo / "lakefile.lean").write_text(
        "import Lake\nopen Lake DSL\n\npackage tablet\n\n"
        "lean_lib Tablet where\n  globs := #[.submodules `Tablet]\n"
    )
    (repo / "Tablet" / "Preamble.lean").write_text("-- rig preamble\n")
    (repo / "Tablet" / "Preamble.tex").write_text("")
    (repo / "Tablet" / "Rung.lean").write_text(NODE_FILE)
    (repo / "Tablet" / "Rung.tex").write_text(NODE_TEX)
    (repo / "Tablet" / "Lift.lean").write_text(LIFT_FILE)
    (repo / "Tablet" / "Lift.tex").write_text(LIFT_TEX)
    (repo / "APPROVED_AXIOMS.json").write_text('{"global": [], "nodes": {}}\n')
    scripts = repo / "scripts"
    scripts.mkdir(exist_ok=True)
    for name in ("lean_local_closure.lean", "lean_semantic_fingerprint.lean"):
        shutil.copy2(REPO_ROOT / "scripts" / name, scripts / name)
    check = repo / ".trellis" / "scripts" / "check.py"
    check.parent.mkdir(parents=True, exist_ok=True)
    check.write_text(CHECK_PY_SHIM)
    check.chmod(0o755)


def _lake_build(repo: Path, target: str) -> None:
    proc = subprocess.run(
        ["lake", "build", target],
        cwd=str(repo),
        capture_output=True,
        text=True,
        timeout=600,
    )
    assert proc.returncode == 0, proc.stdout + proc.stderr


SIDECAR_CONFIG = {
    "repo_path": ".",
    "worker": {"provider": "codex", "model": "w", "label": "w"},
    "reviewer": {"provider": "codex", "model": "r", "label": "r"},
    "verification": {
        "correspondence_agents": [
            {"provider": "codex", "model": "m", "label": "corr-a"}
        ],
        "soundness_agents": [
            {"provider": "codex", "model": "m", "label": "sound-a"}
        ]
    },
    "workflow": {
        "main_result_targets": [
            {"start_line": 1, "end_line": 2, "tex_label": "thm:rung"}
        ]
    },
    "sidecar": {
        "enabled": True,
        "model": {"provider": "mistral", "name": "labs-leanstral-1-5"},
        "budgets": {"max_iterations": 4, "attempt_wall_seconds": 300},
        "apply": {"budget_seconds": 300, "max_applies_per_boundary": 1},
        "phases": {"proof_formalization": True, "stating_after_coverage": True},
        "daemon": {"sandbox_role": "", "lean_threads": 2},
    }
}


@pytest.fixture(scope="module")
def rig() -> Dict[str, Any]:
    base = Path(tempfile.mkdtemp(prefix="scrig-", dir=str(Path.home() / ".cache")))
    worker = base / "r"
    _seed_tablet(worker)
    (worker / ".gitignore").write_text(".lake/\n.trellis/\n")
    _lake_build(worker, "Tablet.Rung")
    _lake_build(worker, "Tablet.Lift")
    _git(worker, "init", "-q")
    _git(worker, "add", "-A")
    _git(worker, "commit", "-qm", "seed rig")
    _git(worker, "tag", "supervisor2/checkpoint-000001")
    worker_head = _git(worker, "rev-parse", "HEAD")

    # Supervisor (authoritative) repo in the production layout.
    supervisor = worker / ".trellis" / "supervisor" / "repo"
    supervisor.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "clone", "-q", "--no-tags", str(worker), str(supervisor)],
        check=True,
        capture_output=True,
    )
    # The clone lacks the untracked .trellis bits; re-seed its check.py
    # and config, and give it its own built .lake.
    _seed_tablet(supervisor)  # idempotent rewrite of the same content
    (supervisor / "trellis.config.json").write_text(json.dumps(SIDECAR_CONFIG))
    (worker / "trellis.config.json").write_text(json.dumps(SIDECAR_CONFIG))
    _lake_build(supervisor, "Tablet.Rung")
    _lake_build(supervisor, "Tablet.Lift")
    _git(supervisor, "add", "-A")
    _git(supervisor, "commit", "-qm", "supervisor baseline")

    runtime = worker / ".trellis" / "runtime" / "rt"
    runtime.mkdir(parents=True, exist_ok=True)

    # REAL checker server over the rig (A6).
    from trellis.checker.server import CheckerServer

    server = CheckerServer(runtime, parallelism=2, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    yield {
        "base": base,
        "worker": worker,
        "supervisor": supervisor,
        "runtime": runtime,
        "server": server,
        "worker_head": worker_head,
        "socket": server.socket_path,
    }

    server.shutdown()
    thread.join(timeout=5.0)
    shutil.rmtree(base, ignore_errors=True)


def _initial_state(queue: List[Dict[str, Any]] | None = None) -> Dict[str, Any]:
    # Queue redesign: the apply gate requires the node queued with a
    # matching generation; the rig defaults to Rung queued at seq 1.
    queue = (
        [{"node": "Rung", "entry_seq": 1, "queued_at_cycle": 1}]
        if queue is None
        else queue
    )
    return {
        "phase": "ProofFormalization",
        "cycle": 1,
        "live": {
            "present_nodes": ["Preamble", "Rung", "Lift"],
            "open_nodes": ["Rung", "Lift"],
        },
        "committed": {
            "present_nodes": ["Preamble", "Rung", "Lift"],
            "open_nodes": ["Rung", "Lift"],
        },
        "proof_nodes": ["Rung", "Lift"],
        "node_kinds": {"Preamble": "Preamble", "Rung": "Proof", "Lift": "Proof"},
        "corr_status": {"Rung": "Pass", "Lift": "Pass"},
        "corr_approved_fingerprints": {"Rung": "rig-bootstrap", "Lift": "rig-lift"},
        "substantiveness_status": {"Rung": "Pass", "Lift": "Pass"},
        "substantiveness_approved_fingerprints": {"Rung": "s1", "Lift": "s2"},
        "sidecar_queue": queue,
        "sidecar_queue_seq": max([e["entry_seq"] for e in queue], default=0),
    }


def _run_kernel(
    rig: Dict[str, Any],
    request: Dict[str, Any],
    *,
    allow_post_hook_step_failure: bool = False,
) -> Dict[str, Any]:
    proc = subprocess.run(
        [str(_kernel_bin())],
        input=json.dumps(request),
        capture_output=True,
        text=True,
        timeout=600,
        env={
            **os.environ,
            "TRELLIS_CHECKER_SOCKET": str(rig["socket"]),
        },
    )
    if proc.returncode != 0 and allow_post_hook_step_failure:
        # The boundary hook runs (and durably persists its apply)
        # BEFORE step_runtime; the FOLLOW-ON StartCycle's worker-
        # dispatch prep legitimately needs mathlib tooling
        # (`lake exe cache get`) the micro-workspace does not ship.
        # Only that class of failure is tolerated.
        assert "runtime step failed" in proc.stdout + proc.stderr, (
            f"unexpected kernel failure: {proc.stdout}\n{proc.stderr}"
        )
        return {"stdout": proc.stdout, "stderr": proc.stderr}
    assert proc.returncode == 0, f"kernel failed: {proc.stdout}\n{proc.stderr}"
    return {"stdout": proc.stdout, "stderr": proc.stderr}


def _init_runtime(rig: Dict[str, Any], queue: List[Dict[str, Any]] | None = None) -> None:
    # Non-initial (cycle 1) states require a non-empty event-log dir at
    # load; seed the cycle-1 segment with its StartCycle record.
    event_dir = rig["worker"] / ".trellis-history" / "event-log"
    event_dir.mkdir(parents=True, exist_ok=True)
    (event_dir / "cycle-000001.jsonl").write_text(
        json.dumps(
            {
                "index": 0,
                "event": {"event": "start_cycle"},
                "commands": [],
                "phase": "ProofFormalization",
                "stage": "Start",
                "cycle": 1,
                "ts_ms": 0,
            }
        )
        + "\n"
    )
    state = _initial_state(queue)
    # Seed the substantiveness live fingerprints equal to approved so
    # the current lane state is Pass.
    lane = {"Rung": "s1", "Lift": "s2"}
    corr = {"Rung": "rig-bootstrap", "Lift": "rig-lift"}
    state["live"]["substantiveness_current_fingerprints"] = dict(lane)
    state["committed"]["substantiveness_current_fingerprints"] = dict(lane)
    state["live"]["corr_current_fingerprints"] = dict(corr)
    state["committed"]["corr_current_fingerprints"] = dict(corr)
    _run_kernel(
        rig,
        {
            "action": "init",
            "root": str(rig["runtime"]),
            "state": state,
            "metadata": {
                "repo_path": str(rig["worker"]),
                "config_path": str(rig["worker"] / "trellis.config.json"),
                "native_history_kinds": [],
                "initial_planning_seeded": False,
                "coverage_replanning_seeded": False,
            },
        },
    )


def _publish_rig_attempt(
    runtime: Path,
    supervisor: Path,
    *,
    body: str,
    node_file_sha: str | None = None,
    entry_seq: int = 1,
) -> str:
    from trellis.sidecar.spool import ensure_spool_dirs, publish_attempt, spool_dirs

    pre_image = (supervisor / "Tablet" / "Rung.lean").read_text()
    attempt_id = f"rig-{int(time.time() * 1000)}"
    record = {
        "schema": 2,
        "attempt_id": attempt_id,
        "node": "Rung",
        "snapshot_sha": "rig",
        "entry_seq": entry_seq,
        "base": {
            "node_file_sha256": node_file_sha or _sha256(pre_image),
            "statement_prefix_sha256": "sp",
        },
        "artifact": {"proof_body": body},
        "status": "success",
        "provenance": {
            "provider": "mistral",
            "model": "labs-leanstral-1-5",
            "iterations": 2,
            "wall_secs": 12.5,
            "tokens": {"prompt": 100, "completion": 20},
            "driver_version": "rig",
        },
        "daemon_validation": {"compiled": True},
    }
    dirs = spool_dirs(runtime)
    ensure_spool_dirs(dirs)
    publish_attempt(dirs, record)
    return attempt_id


# ---------------------------------------------------------------------------
# Daemon half (mocked model, real workspace + compile loop + prevalidate)
# ---------------------------------------------------------------------------


class ScriptedClient:
    def __init__(self, bodies: List[str]) -> None:
        self._bodies = list(bodies)

    def chat(self, messages, tools=None, **_kwargs):
        body = self._bodies.pop(0) if self._bodies else "  sorry\n"
        return {
            "usage": {"prompt_tokens": 10, "completion_tokens": 5},
            "choices": [
                {
                    "message": {
                        "content": "",
                        "tool_calls": [
                            {
                                "id": "c1",
                                "function": {
                                    "name": "lean_run_code",
                                    "arguments": json.dumps({"body": body}),
                                },
                            }
                        ],
                    }
                }
            ],
        }


def _run_rig_attempt(
    daemon_runtime: Path,
    config,
    snapshot_sha: str,
    *,
    grunt: int = 0,
    entry_seq: int = 1,
    attempt_id: str = "sc-rig-grunt",
) -> Dict[str, Any]:
    from trellis.sidecar.attempt import run_grunt_attempt

    return run_grunt_attempt(
        daemon_runtime,
        grunt=grunt,
        node="Rung",
        entry_seq=entry_seq,
        snapshot_sha=snapshot_sha,
        node_file_sha=_sha256(NODE_FILE),
        statement_prefix_sha="sp",
        attempt_id=attempt_id,
        config=config,
    )


def test_grunt_attempt_good_and_bad_runs(rig: Dict[str, Any], monkeypatch) -> None:
    from trellis.sidecar import attempt as attempt_mod
    from trellis.sidecar.config import SidecarConfig
    from trellis.sidecar.spool import pending_attempt_ids, spool_dirs
    from trellis.sidecar.workspace import bootstrap_workspace

    daemon_runtime = rig["base"] / "daemon-runtime"
    daemon_runtime.mkdir(exist_ok=True)
    config = SidecarConfig(
        enabled=True,
        sandbox_role="",
        lean_threads=2,
        max_iterations=3,
        attempt_wall_seconds=300.0,
    )
    bootstrap_workspace(rig["worker"], daemon_runtime, grunt=0)

    monkeypatch.setenv("INCREMENTAL_CHECK_ALLOW_UNSANDBOXED", "1")
    monkeypatch.setattr(attempt_mod, "read_api_key", lambda cfg: "sk-rig")

    # Good run: known-good body ⇒ exactly one pending/ record carrying
    # the queue generation (A3).
    monkeypatch.setattr(
        attempt_mod, "ModelClient", lambda cfg, key, **_kw: ScriptedClient(["  rfl\n"])
    )
    outcome = _run_rig_attempt(
        daemon_runtime, config, rig["worker_head"], attempt_id="sc-rig-good"
    )
    assert outcome["status"] == "success", outcome
    dirs = spool_dirs(daemon_runtime)
    assert pending_attempt_ids(dirs) == ["sc-rig-good"]
    record = json.loads(
        (dirs.pending / "attempt-sc-rig-good.json").read_text()
    )
    assert record["entry_seq"] == 1
    assert record["schema"] == 2

    # Bad run: known-bad body ⇒ budget exhausts, NO new pending record.
    monkeypatch.setattr(
        attempt_mod,
        "ModelClient",
        lambda cfg, key, **_kw: ScriptedClient(["  exact bogus_xyz\n"] * 5),
    )
    outcome = _run_rig_attempt(
        daemon_runtime, config, rig["worker_head"], attempt_id="sc-rig-bad"
    )
    assert outcome["status"] in ("budget_exhausted", "failed")
    assert pending_attempt_ids(dirs) == ["sc-rig-good"], (
        "bad run must publish nothing"
    )


class InfoScriptedClient:
    """Scripted client: first turn calls read_file on the node's tex,
    second turn greps the tablet (v4), then submits scripted bodies.
    Captures the advertised tools and the tool results it was fed."""

    def __init__(self, bodies: List[str]) -> None:
        self._bodies = list(bodies)
        self._read_issued = False
        self._search_issued = False
        self.advertised_tools: List[List[str]] = []
        self.tool_results: List[str] = []

    def chat(self, messages, tools=None, **_kwargs):
        self.advertised_tools.append(
            [t["function"]["name"] for t in (tools or [])]
        )
        self.tool_results.extend(
            str(m.get("content", ""))
            for m in messages
            if m.get("role") == "tool"
        )
        if not self._read_issued:
            self._read_issued = True
            call = {
                "id": "read-1",
                "function": {
                    "name": "read_file",
                    "arguments": json.dumps({"path": "Tablet/Rung.tex"}),
                },
            }
        elif not self._search_issued:
            self._search_issued = True
            call = {
                "id": "search-1",
                "function": {
                    "name": "search_tablet",
                    "arguments": json.dumps({"query": "theorem"}),
                },
            }
        else:
            body = self._bodies.pop(0) if self._bodies else "  sorry\n"
            call = {
                "id": "lean-1",
                "function": {
                    "name": "lean_run_code",
                    "arguments": json.dumps({"body": body}),
                },
            }
        return {
            "usage": {"prompt_tokens": 10, "completion_tokens": 5},
            "choices": [
                {"message": {"content": "", "tool_calls": [call]}}
            ],
        }


def test_grunt_attempt_v2_reads_tex_through_real_workspace(
    rig: Dict[str, Any], monkeypatch
) -> None:
    """v2 tools leg: the driver loop serves the node's on-disk prose
    proof through read_file (real workspace file), the info turn costs
    no compile iteration, and with loogle unconfigured
    (`loogle_enabled=False`) search_mathlib is absent from the
    advertised tools. The close still publishes, with phase timings in
    the outcome."""
    from trellis.sidecar import attempt as attempt_mod
    from trellis.sidecar.config import SidecarConfig
    from trellis.sidecar.spool import pending_attempt_ids, spool_dirs
    from trellis.sidecar.workspace import bootstrap_workspace

    daemon_runtime = rig["base"] / "daemon-runtime-v2"
    daemon_runtime.mkdir(exist_ok=True)
    config = SidecarConfig(
        enabled=True,
        sandbox_role="",
        lean_threads=2,
        max_iterations=3,
        attempt_wall_seconds=300.0,
        loogle_enabled=False,
    )
    bootstrap_workspace(rig["worker"], daemon_runtime, grunt=0)

    monkeypatch.setenv("INCREMENTAL_CHECK_ALLOW_UNSANDBOXED", "1")
    monkeypatch.setattr(attempt_mod, "read_api_key", lambda cfg: "sk-rig")
    client = InfoScriptedClient(["  rfl\n"])
    monkeypatch.setattr(attempt_mod, "ModelClient", lambda cfg, key, **_kw: client)

    outcome = _run_rig_attempt(
        daemon_runtime, config, rig["worker_head"], attempt_id="sc-rig-v2"
    )
    assert outcome["status"] == "success", outcome
    # The REAL workspace tex flowed back through the tool turn.
    assert any("thm:rung" in r for r in client.tool_results), client.tool_results
    # v4: the tablet grep answered over the real workspace's Tablet/.
    assert any("Tablet/Rung.lean:" in r for r in client.tool_results), (
        client.tool_results
    )
    # Tool-absent config: loogle unconfigured and no mathlib checkout in
    # the rig, so read_file + search_tablet + get_goals are advertised.
    assert client.advertised_tools[0] == [
        "lean_run_code",
        "read_file",
        "search_tablet",
        "get_goals",
    ]
    # Info turns cost no compile iteration.
    assert outcome["iterations"] == 1
    # v4 telemetry: per-tool usage rides the outcome.
    assert outcome["info_tool_counts"] == {"read_file": 1, "search_tablet": 1}
    assert outcome["info_tool_calls"] == 2
    # Phase telemetry rides the outcome (server open is always timed).
    assert "server_open_secs" in outcome.get("timings", {}), outcome
    # The published record carries the timings additively.
    dirs = spool_dirs(daemon_runtime)
    assert "sc-rig-v2" in pending_attempt_ids(dirs)
    record = json.loads((dirs.pending / "attempt-sc-rig-v2.json").read_text())
    assert record["provenance"]["driver_version"] == "sidecar-driver-v3"
    assert "server_open_secs" in record["provenance"]["timings"]


def test_grunt_attempt_post_loop_failure_is_contained_with_tokens(
    rig: Dict[str, Any], monkeypatch
) -> None:
    """Ledger-loss pin: an exception AFTER the model loop spent tokens
    (prevalidate/record/publish) must surface as an `error` outcome
    that still carries the token counts — the manager ledgers it — and
    must publish nothing to the spool."""
    from trellis.sidecar import attempt as attempt_mod
    from trellis.sidecar import prevalidate as prevalidate_mod
    from trellis.sidecar.config import SidecarConfig
    from trellis.sidecar.spool import pending_attempt_ids, spool_dirs
    from trellis.sidecar.workspace import bootstrap_workspace

    daemon_runtime = rig["base"] / "daemon-runtime-postloop"
    daemon_runtime.mkdir(exist_ok=True)
    config = SidecarConfig(
        enabled=True,
        sandbox_role="",
        lean_threads=2,
        max_iterations=3,
        attempt_wall_seconds=300.0,
    )
    bootstrap_workspace(rig["worker"], daemon_runtime, grunt=0)

    monkeypatch.setenv("INCREMENTAL_CHECK_ALLOW_UNSANDBOXED", "1")
    monkeypatch.setattr(attempt_mod, "read_api_key", lambda cfg: "sk-rig")
    monkeypatch.setattr(
        attempt_mod, "ModelClient", lambda cfg, key, **_kw: ScriptedClient(["  rfl\n"])
    )

    def boom(**kwargs):
        raise RuntimeError("prevalidate crashed")

    monkeypatch.setattr(prevalidate_mod, "prevalidate_success", boom)
    outcome = _run_rig_attempt(daemon_runtime, config, rig["worker_head"])
    assert outcome["status"] == "error"
    assert outcome["detail"].startswith("post-loop failure")
    assert outcome["prompt_tokens"] > 0, "spent tokens must survive the crash"
    assert pending_attempt_ids(spool_dirs(daemon_runtime)) == []


def test_failing_attempt_outcome_carries_v3_telemetry(
    rig: Dict[str, Any], monkeypatch
) -> None:
    """Combined-audit F5: the v3 long-regime telemetry (compactions,
    compaction round tokens, transport retries, effort in force) rides
    on EVERY outcome, not only the published success record —
    ``build_attempt_record`` is reached on success alone, so a failing
    attempt used to carry none of it, and failures are exactly the
    population you diagnose from."""
    from trellis.sidecar import attempt as attempt_mod
    from trellis.sidecar.config import SidecarConfig
    from trellis.sidecar.spool import pending_attempt_ids, spool_dirs
    from trellis.sidecar.workspace import bootstrap_workspace

    daemon_runtime = rig["base"] / "daemon-runtime-telemetry"
    daemon_runtime.mkdir(exist_ok=True)
    config = SidecarConfig(
        enabled=True,
        sandbox_role="",
        lean_threads=2,
        max_iterations=2,
        attempt_wall_seconds=300.0,
        reasoning_effort="high",
    )
    bootstrap_workspace(rig["worker"], daemon_runtime, grunt=0)

    monkeypatch.setenv("INCREMENTAL_CHECK_ALLOW_UNSANDBOXED", "1")
    monkeypatch.setattr(attempt_mod, "read_api_key", lambda cfg: "sk-rig")
    monkeypatch.setattr(
        attempt_mod,
        "ModelClient",
        lambda cfg, key, **_kw: ScriptedClient(["  exact bogus_xyz\n"] * 4),
    )
    outcome = _run_rig_attempt(
        daemon_runtime, config, rig["worker_head"], attempt_id="sc-rig-telemetry"
    )
    assert outcome["status"] in ("budget_exhausted", "failed"), outcome
    assert outcome["compactions"] == 0
    assert outcome["compaction_round_tokens"] == []
    assert outcome["transport_retries"] == 0
    assert outcome["reasoning_effort"] == "high"
    assert pending_attempt_ids(spool_dirs(daemon_runtime)) == []


# ---------------------------------------------------------------------------
# Kernel half (real checker socket — A6)
# ---------------------------------------------------------------------------


def test_run_loop_config_off_creates_no_sidecar_surface(rig: Dict[str, Any]) -> None:
    """Integrated config-off pin (the audit's fails-if-broken gap): a
    real `run` invocation with NO `sidecar` config block must create no
    ``<runtime>/sidecar/`` directory and emit zero sidecar log lines —
    the operational form of the byte-identical inertness guarantee."""
    worker: Path = rig["worker"]
    runtime: Path = rig["runtime"]
    config_off = {k: v for k, v in SIDECAR_CONFIG.items() if k != "sidecar"}
    (worker / "trellis.config.json").write_text(json.dumps(config_off))
    try:
        _init_runtime(rig)
        result = _run_kernel(
            rig,
            {"action": "run", "root": str(runtime), "max_steps": 1},
            allow_post_hook_step_failure=True,
        )
        assert not (runtime / "sidecar").exists(), (
            "config-off run must not create <runtime>/sidecar/"
        )
        assert "trellis sidecar:" not in result["stdout"] + result["stderr"], (
            "config-off run must emit zero sidecar log lines"
        )
    finally:
        # Restore the rig baseline for the apply tests below.
        (worker / "trellis.config.json").write_text(json.dumps(SIDECAR_CONFIG))
        shutil.rmtree(worker / ".trellis-history", ignore_errors=True)


def test_kernel_rejects_stale_generation_record(rig: Dict[str, Any]) -> None:
    """A3/A9 publish-cancel race leg: a record carrying a superseded
    queue generation dies as ``rejected/stale_generation`` with the
    worktree untouched and the queue entry intact."""
    runtime: Path = rig["runtime"]
    worker: Path = rig["worker"]
    _init_runtime(rig)  # Rung queued at seq 1
    node_path = worker / "Tablet" / "Rung.lean"
    before = node_path.read_text()
    _publish_rig_attempt(runtime, worker, body="  rfl\n", entry_seq=99)
    _run_kernel(
        rig,
        {"action": "run", "root": str(runtime), "max_steps": 1},
        allow_post_hook_step_failure=True,
    )
    rejected_dir = runtime / "sidecar" / "spool" / "rejected"
    rejected = sorted(rejected_dir.glob("*.json"), key=lambda p: p.stat().st_mtime)
    assert rejected, "expected a rejected record"
    verdict = json.loads(rejected[-1].read_text())["verdict"]
    assert verdict["outcome"] == "rejected"
    assert verdict["reason"].startswith("stale_generation"), verdict
    assert node_path.read_text() == before, "worktree must be untouched"
    export = json.loads((runtime / "sidecar" / "candidates.json").read_text())
    rung = next(row for row in export["queue"] if row["node"] == "Rung")
    assert rung["entry_seq"] == 1 and rung["status"] == "ready", (
        "the live generation stays queued"
    )
    shutil.rmtree(worker / ".trellis-history", ignore_errors=True)


def test_manager_n2_assigns_from_real_export_and_cancels_on_removal(
    rig: Dict[str, Any],
) -> None:
    """E2E N=2 + queue-remove → cancel: BOTH exports are produced by
    the real kernel binary from real state (queued {Rung, Lift}, then a
    re-init without Lift — the post-remove state image); the manager
    consumes them verbatim, fills both grunts, then kills Lift's
    attempt process group and resets that grunt's workspace."""
    from trellis.sidecar.config import SidecarConfig
    from trellis.sidecar.daemon import GruntSlot, SidecarDaemon

    runtime: Path = rig["runtime"]
    _init_runtime(
        rig,
        queue=[
            {"node": "Rung", "entry_seq": 3, "queued_at_cycle": 1},
            {"node": "Lift", "entry_seq": 4, "queued_at_cycle": 1},
        ],
    )
    _run_kernel(
        rig,
        {"action": "run", "root": str(runtime), "max_steps": 1},
        allow_post_hook_step_failure=True,
    )
    export = json.loads((runtime / "sidecar" / "candidates.json").read_text())
    ready = {row["node"]: row for row in export["queue"] if row["status"] == "ready"}
    assert set(ready) == {"Rung", "Lift"}, export["queue"]

    env = rig["base"] / "manager-env"
    env.write_text("MISTRAL_API_KEY=sk-rig\n")
    config = SidecarConfig(
        enabled=True,
        api_key_env_file=str(env),
        poll_seconds=0.01,
        export_stale_after_seconds=3600.0,
    )
    procs: Dict[str, subprocess.Popen] = {}
    resets: List[tuple] = []

    def spawn(grunt, row, export_doc, attempt_id):
        proc = subprocess.Popen(["sleep", "120"], start_new_session=True)
        procs[str(row["node"])] = proc
        return GruntSlot(
            grunt=grunt,
            node=str(row["node"]),
            entry_seq=int(row["entry_seq"]),
            attempt_id=attempt_id,
            started_at_ms=0,
            proc=proc,
            result_path=rig["base"] / f"result-{attempt_id}.json",
        )

    daemon = SidecarDaemon(
        runtime_root=runtime,
        config=config,
        spawn_fn=spawn,
        reset_workspace_fn=lambda k, sha: resets.append((k, sha)),
        log=lambda line: None,
        kill_grace_seconds=1.0,
    )
    try:
        assert daemon.run_once() == "assigned"
        busy = {s.node: s for s in daemon.slots.values() if s is not None}
        assert set(busy) == {"Rung", "Lift"}, "N=2: both grunts filled"
        assert {s.grunt for s in busy.values()} == {0, 1}

        # Kernel-authored remove image: re-init WITHOUT Lift queued and
        # let the real binary export the post-remove queue.
        shutil.rmtree(rig["worker"] / ".trellis-history", ignore_errors=True)
        _init_runtime(
            rig, queue=[{"node": "Rung", "entry_seq": 3, "queued_at_cycle": 1}]
        )
        _run_kernel(
            rig,
            {"action": "run", "root": str(runtime), "max_steps": 1},
            allow_post_hook_step_failure=True,
        )
        export = json.loads((runtime / "sidecar" / "candidates.json").read_text())
        assert all(row["node"] != "Lift" for row in export["queue"])

        daemon.run_once()
        assert procs["Lift"].poll() is not None, (
            "Lift's attempt process group must be dead"
        )
        assert procs["Rung"].poll() is None, "Rung's attempt keeps running"
        lift_grunt = busy["Lift"].grunt
        assert any(k == lift_grunt for k, _ in resets), "workspace reset invoked"
        survivors = {s.node for s in daemon.slots.values() if s is not None}
        assert survivors == {"Rung"}
        rows = [
            json.loads(line)
            for line in (runtime / "sidecar" / "ledger.jsonl")
            .read_text()
            .splitlines()
        ]
        assert any(
            r.get("status") == "cancelled" and r.get("node") == "Lift" for r in rows
        )
    finally:
        for proc in procs.values():
            try:
                os.killpg(os.getpgid(proc.pid), 9)
            except (OSError, ProcessLookupError):
                pass
        shutil.rmtree(rig["worker"] / ".trellis-history", ignore_errors=True)
        (runtime / "sidecar" / "ledger.jsonl").unlink(missing_ok=True)
        (runtime / "sidecar" / "manager_cursor.json").unlink(missing_ok=True)


def test_drain_restart_adopt_and_expire_one_generation_end_to_end(
    rig: Dict[str, Any],
) -> None:
    """The whole restart-across-a-live-pool story against the REAL
    kernel: manager A assigns from a real export and DRAINS (its child
    keeps running), manager B adopts that child off ``slots.json`` and
    assigns nothing, the attempt then finishes and is reaped once — and
    the kernel expires EXACTLY ONE generation and applies no closure.

    Without adoption this same sequence double-attempts the generation:
    the drained attempt leaves no attempted.json row for manager B to
    dedupe against."""
    from test_sidecar_adoption import _live_attempt_child

    from trellis.sidecar.config import SidecarConfig
    from trellis.sidecar.daemon import (
        GruntSlot,
        SidecarDaemon,
        drain_sentinel_path,
        load_attempted,
        load_slots,
    )

    runtime: Path = rig["runtime"]
    worker: Path = rig["worker"]
    for name in ("ledger.jsonl", "manager_cursor.json", "slots.json",
                 "attempted.json", "status.json"):
        (runtime / "sidecar" / name).unlink(missing_ok=True)
    _init_runtime(rig, queue=[{"node": "Rung", "entry_seq": 3, "queued_at_cycle": 1}])
    _clear_spool(runtime)
    _run_kernel(
        rig,
        {"action": "run", "root": str(runtime), "max_steps": 1},
        allow_post_hook_step_failure=True,
    )
    export = json.loads((runtime / "sidecar" / "candidates.json").read_text())
    assert [row["node"] for row in export["queue"] if row["status"] == "ready"] == [
        "Rung"
    ]

    env = rig["base"] / "adopt-env"
    env.write_text("MISTRAL_API_KEY=sk-rig\n")
    config = SidecarConfig(
        enabled=True,
        api_key_env_file=str(env),
        poll_seconds=0.01,
        export_stale_after_seconds=3600.0,
    )
    proc: subprocess.Popen | None = None
    result_path = rig["base"] / "adopt-result.json"
    result_path.unlink(missing_ok=True)

    def spawn(grunt, row, export_doc, attempt_id):
        nonlocal proc
        proc = _live_attempt_child(
            runtime,
            grunt=grunt,
            node=str(row["node"]),
            entry_seq=int(row["entry_seq"]),
            attempt_id=attempt_id,
            result_path=str(result_path),
        )
        return GruntSlot(
            grunt=grunt,
            node=str(row["node"]),
            entry_seq=int(row["entry_seq"]),
            attempt_id=attempt_id,
            started_at_ms=1_700_000_000_000,
            proc=proc,
            result_path=result_path,
            assigned_at_cycle=int(export_doc.get("cycle", 0) or 0),
        )

    def _manager(spawn_fn) -> SidecarDaemon:
        return SidecarDaemon(
            runtime_root=runtime,
            config=config,
            spawn_fn=spawn_fn,
            log=lambda line: None,
            kill_grace_seconds=1.0,
        )

    try:
        manager_a = _manager(spawn)
        manager_a.startup()
        assert manager_a.run_once() == "assigned"
        slot_a = next(s for s in manager_a.slots.values() if s is not None)
        assert proc is not None and proc.poll() is None

        drain_sentinel_path(runtime).touch()
        assert manager_a.run_once() == "drain"
        drain_sentinel_path(runtime).unlink()
        assert proc.poll() is None, "the drained attempt keeps running"
        assert [r["attempt_id"] for r in load_slots(runtime)] == [slot_a.attempt_id]

        # Manager B: a fresh process over the same runtime root.
        spawned_by_b: List[str] = []

        def refuse(grunt, row, export_doc, attempt_id):  # pragma: no cover
            spawned_by_b.append(attempt_id)
            raise AssertionError("manager B must not re-attempt the live generation")

        manager_b = _manager(refuse)
        manager_b.startup()
        adopted = next(s for s in manager_b.slots.values() if s is not None)
        assert adopted.attempt_id == slot_a.attempt_id
        assert adopted.adopted is True
        assert adopted.started_at_ms == 1_700_000_000_000, "wall clock is not restamped"
        assert adopted.assigned_at_cycle == slot_a.assigned_at_cycle
        assert manager_b.run_once() == "idle" and spawned_by_b == []

        # The adopted attempt finishes under manager B and is reaped.
        result_path.write_text(
            json.dumps(
                {
                    "attempt_id": adopted.attempt_id,
                    "status": "failed",
                    "detail": "compile: unsolved goals",
                }
            )
        )
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        deadline = time.time() + 10.0
        while proc.poll() is None and time.time() < deadline:
            time.sleep(0.05)
        manager_b.run_once()
        assert load_attempted(runtime)["Rung"][0]["entry_seq"] == 3
        assert load_slots(runtime) == []
        outcomes = list((runtime / "sidecar" / "spool" / "outcomes").glob("*.json"))
        assert len(outcomes) == 1, "exactly one spent-generation report"

        _run_kernel(
            rig,
            {"action": "run", "root": str(runtime), "max_steps": 1},
            allow_post_hook_step_failure=True,
        )
        export = json.loads((runtime / "sidecar" / "candidates.json").read_text())
        assert [row["node"] for row in export["queue"]] == [], (
            "the one adopted generation expired"
        )
        pruned = export["pruned_recent"][-1]
        assert (pruned["node"], pruned["entry_seq"]) == ("Rung", 3)
        assert pruned["reason"] == "attempt_spent:failed", pruned
        consumed = list(
            (runtime / "sidecar" / "spool" / "outcomes_consumed").glob("outcome-*.json")
        )
        assert len(consumed) == 1, consumed
        # A failing attempt closes nothing: no closure was applied.
        assert not list((runtime / "sidecar" / "spool" / "applied").glob("*.json"))
    finally:
        if proc is not None:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except (OSError, ProcessLookupError):
                pass
        shutil.rmtree(worker / ".trellis-history", ignore_errors=True)
        for name in ("ledger.jsonl", "manager_cursor.json", "slots.json",
                     "attempted.json", "status.json"):
            (runtime / "sidecar" / name).unlink(missing_ok=True)


def test_kernel_ingest_applies_through_real_checker(rig: Dict[str, Any]) -> None:
    runtime: Path = rig["runtime"]
    supervisor: Path = rig["supervisor"]
    worker: Path = rig["worker"]
    _init_runtime(rig)

    pre_image = (worker / "Tablet" / "Rung.lean").read_text()
    _publish_rig_attempt(runtime, worker, body="  rfl\n")

    result = _run_kernel(
        rig,
        {"action": "run", "root": str(runtime), "max_steps": 1},
        allow_post_hook_step_failure=True,
    )

    # Spool: applied with a verdict.
    applied_dir = runtime / "sidecar" / "spool" / "applied"
    applied = list(applied_dir.glob("*.json"))
    rejected_dir = runtime / "sidecar" / "spool" / "rejected"
    rejected = list(rejected_dir.glob("*.json"))
    assert applied, (
        f"expected an applied record; rejected={[p.name for p in rejected]} "
        f"reasons={[json.loads(p.read_text()).get('verdict') for p in rejected]} "
        f"stderr tail: {result['stderr'][-2000:]}"
    )
    verdict = json.loads(applied[0].read_text())["verdict"]
    assert verdict["outcome"] == "applied"

    # Worktree: node closed in the RUN (worker) repo and MIRRORED into
    # the supervisor workspace (the checker's compile sandbox).
    closed = (worker / "Tablet" / "Rung.lean").read_text()
    assert closed.endswith("  rfl\n") and "sorry" not in closed
    assert (supervisor / "Tablet" / "Rung.lean").read_text() == closed

    # Event log: the sidecar_closure event landed.
    event_dir = worker / ".trellis-history" / "event-log"
    log_text = "".join(
        p.read_text() for p in sorted(event_dir.glob("cycle-*.jsonl"))
    )
    assert '"sidecar_closure"' in log_text, (
        f"files={sorted(p.name for p in event_dir.glob('*'))} "
        f"exists={event_dir.exists()} stderr={result['stderr'][-1500:]}"
    )

    # State: provenance recorded, node no longer open.
    state = json.loads((runtime / "protocol_state.json").read_text())
    provenance = state.get("closure_provenance", {})
    assert provenance.get("Rung", {}).get("closed_by") == "sidecar"
    assert "Rung" not in state["live"]["open_nodes"]
    record = state["local_closure_records"]["Rung"]
    assert record["kernel_axioms"] == [] or set(record["kernel_axioms"]) <= {
        "propext",
        "funext",
        "Classical.choice",
        "Quot.sound",
    }
    assert record["toolchain_hash"], "record must carry real hashes, not sentinels"
    assert "TODO" not in record["toolchain_hash"]

    # Journal cleared.
    assert not (runtime / "sidecar" / "apply-journal.json").exists()

    # A9 ordering: the export reflects POST-apply state (the queue entry
    # was consumed by the apply; the closed node is no longer eligible).
    export = json.loads((runtime / "sidecar" / "candidates.json").read_text())
    assert export["schema"] == 2
    assert all(row["node"] != "Rung" for row in export["queue"])
    assert all(row["node"] != "Rung" for row in export["eligible_now"])

    # A6: the gate traffic hit the REAL checker socket. The server logs
    # every op it served.
    server_log = runtime / "checker-state" / "server.log"
    log = server_log.read_text() if server_log.exists() else ""
    for op in ("lean_compile_node", "local_closure_axioms"):
        assert op in log, f"expected {op} traffic on the checker socket; log:\n{log[-2000:]}"

    # Pre-image restored nowhere: the closure IS the new content.
    assert (worker / "Tablet" / "Rung.lean").read_text() != pre_image


def test_kernel_rejects_doctored_stale_record(rig: Dict[str, Any]) -> None:
    runtime: Path = rig["runtime"]
    worker: Path = rig["worker"]
    node_path = worker / "Tablet" / "Rung.lean"
    closed_content = node_path.read_text()

    _publish_rig_attempt(
        runtime,
        worker,
        body="  exact rfl\n",
        node_file_sha=_sha256("some other pre-image entirely"),
    )
    before = node_path.read_text()
    _run_kernel(
        rig,
        {"action": "run", "root": str(runtime), "max_steps": 1},
        allow_post_hook_step_failure=True,
    )

    rejected_dir = runtime / "sidecar" / "spool" / "rejected"
    rejected = sorted(rejected_dir.glob("*.json"), key=lambda p: p.stat().st_mtime)
    assert rejected, "expected a rejected record"
    verdict = json.loads(rejected[-1].read_text())["verdict"]
    # Closed node ⇒ ineligible; stale content would reject too had the
    # node been open — either way the worktree is untouched.
    assert verdict["outcome"] == "rejected"
    assert verdict["reason"].startswith(("stale_content", "ineligible"))
    assert node_path.read_text() == before, "worktree must be untouched"
    assert closed_content == before


def _clear_spool(runtime: Path) -> None:
    """The rig's runtime dir is shared across tests in this module.
    Start each outcome-lane test from an empty spool (the assertions
    read whole directories) and with no persisted closure record left
    over from an earlier apply — a record for a node the re-initialised
    state re-opens makes EVERY subsequent event fail the closure
    invariant."""
    spool = runtime / "sidecar" / "spool"
    for name in ("outcomes", "claimed_outcomes", "outcomes_consumed",
                 "pending", "claimed", "applied", "rejected"):
        shutil.rmtree(spool / name, ignore_errors=True)
    shutil.rmtree(runtime / "checker-state" / "local-closure-records",
                  ignore_errors=True)


def _publish_rig_outcome(
    runtime: Path,
    *,
    node: str = "Rung",
    entry_seq: int = 1,
    status: str = "failed",
    export_cycle: int = 0,
    attempt_id: str | None = None,
) -> str:
    from trellis.sidecar.spool import ensure_spool_dirs, publish_outcome, spool_dirs

    attempt_id = attempt_id or f"rig-out-{int(time.time() * 1000)}-{node}"
    dirs = spool_dirs(runtime)
    ensure_spool_dirs(dirs)
    publish_outcome(
        dirs,
        {
            "schema": 1,
            "attempt_id": attempt_id,
            "node": node,
            "entry_seq": entry_seq,
            "status": status,
            "detail": "compile: unsolved goals",
            "export_cycle": export_cycle,
            "ts": time.time(),
        },
    )
    return attempt_id


def test_kernel_expires_spent_generation_end_to_end(rig: Dict[str, Any]) -> None:
    """The whole spent-generation lane through the REAL kernel binary:
    the daemon's published outcome is claimed, applied as a
    `sidecar_attempt_outcomes` event, the queue entry leaves the
    exported queue with a named prune-log reason, and the record is
    consumed with a verdict. The worktree is never touched — expiry
    retires dead work, it does not close anything."""
    runtime: Path = rig["runtime"]
    worker: Path = rig["worker"]
    _init_runtime(rig)  # Rung queued at seq 1, Lift unqueued
    _clear_spool(runtime)
    node_path = worker / "Tablet" / "Rung.lean"
    before = node_path.read_text()
    attempt_id = _publish_rig_outcome(runtime, entry_seq=1, export_cycle=1)

    _run_kernel(
        rig,
        {"action": "run", "root": str(runtime), "max_steps": 1},
        allow_post_hook_step_failure=True,
    )

    export = json.loads((runtime / "sidecar" / "candidates.json").read_text())
    assert [row["node"] for row in export["queue"]] == [], (
        "the spent generation left the queue"
    )
    pruned = export["pruned_recent"][-1]
    assert pruned["node"] == "Rung" and pruned["entry_seq"] == 1
    assert pruned["reason"] == "attempt_spent:failed", pruned
    # Rung is eligible again: retirement frees the node to be re-added.
    assert "Rung" in [row["node"] for row in export["eligible_now"]]

    consumed_dir = runtime / "sidecar" / "spool" / "outcomes_consumed"
    consumed = list(consumed_dir.glob("outcome-*.json"))
    assert len(consumed) == 1, consumed
    record = json.loads(consumed[0].read_text())
    assert record["attempt_id"] == attempt_id
    assert record["verdict"]["outcome"] == "expired"
    assert not list((runtime / "sidecar" / "spool" / "outcomes").glob("*.json"))

    assert node_path.read_text() == before, "an expiry never touches the worktree"

    log_text = "\n".join(
        path.read_text()
        for path in sorted(
            (worker / ".trellis-history" / "event-log").glob("cycle-*.jsonl")
        )
    )
    assert '"sidecar_attempt_outcomes"' in log_text, (
        "the expiry is an ordinary logged event"
    )
    shutil.rmtree(worker / ".trellis-history", ignore_errors=True)


def test_kernel_drops_stale_and_rewound_outcomes(rig: Dict[str, Any]) -> None:
    """The two pre-filter drops, through the real binary: a superseded
    generation and a report minted after a cycle the kernel has rewound
    past both die in `outcomes_consumed/` with a named verdict, leaving
    the live queue entry untouched."""
    runtime: Path = rig["runtime"]
    worker: Path = rig["worker"]
    _init_runtime(rig, queue=[{"node": "Rung", "entry_seq": 5, "queued_at_cycle": 1}])
    _clear_spool(runtime)
    _publish_rig_outcome(
        runtime, entry_seq=4, export_cycle=1, attempt_id="rig-stale"
    )
    _publish_rig_outcome(
        runtime, entry_seq=5, export_cycle=99, attempt_id="rig-rewound"
    )

    _run_kernel(
        rig,
        {"action": "run", "root": str(runtime), "max_steps": 1},
        allow_post_hook_step_failure=True,
    )

    verdicts = {
        json.loads(path.read_text())["attempt_id"]:
            json.loads(path.read_text())["verdict"]["outcome"]
        for path in (runtime / "sidecar" / "spool" / "outcomes_consumed").glob("*.json")
    }
    assert verdicts == {
        "rig-stale": "dropped:stale_generation",
        "rig-rewound": "dropped:post_rewind",
    }, verdicts
    export = json.loads((runtime / "sidecar" / "candidates.json").read_text())
    rung = next(row for row in export["queue"] if row["node"] == "Rung")
    assert rung["entry_seq"] == 5, "the live generation is untouched"
    shutil.rmtree(worker / ".trellis-history", ignore_errors=True)


def test_outcome_never_expires_a_node_with_a_closure_awaiting_ingest(
    rig: Dict[str, Any],
) -> None:
    """RISK 1's belt, end to end. A node with a closure still in
    `pending/` keeps its queue entry: expiring it would make the closure
    apply die `not_queued` and destroy completed grunt work. The
    outcome is not consumed either — it returns to the lane and is
    re-evaluated at the next boundary.

    The rig is configured for one apply per boundary, and this node's
    OWN closure is the pending one, so the two lanes race on the same
    entry within a single boundary — which is exactly the case the
    pre-filter has to win."""
    runtime: Path = rig["runtime"]
    worker: Path = rig["worker"]
    _init_runtime(rig)
    _clear_spool(runtime)
    # TWO published closures for the node; the rig applies at most one
    # per boundary, so the second is still sitting in `pending/` when
    # the outcome lane is evaluated. (The first is doctored so it dies
    # at the pre-image gate without closing anything.)
    _publish_rig_attempt(runtime, worker, body="  rfl\n", node_file_sha="wrong")
    time.sleep(0.01)  # `_publish_rig_attempt` keys its id off the ms clock
    _publish_rig_attempt(runtime, worker, body="  rfl\n")
    _publish_rig_outcome(runtime, entry_seq=1, export_cycle=1, attempt_id="rig-race")

    _run_kernel(
        rig,
        {"action": "run", "root": str(runtime), "max_steps": 1},
        allow_post_hook_step_failure=True,
    )

    # The outcome was NOT consumed: it is back in the lane, byte-intact.
    outcomes = list((runtime / "sidecar" / "spool" / "outcomes").glob("*.json"))
    assert len(outcomes) == 1, outcomes
    assert json.loads(outcomes[0].read_text())["attempt_id"] == "rig-race"
    assert not list(
        (runtime / "sidecar" / "spool" / "outcomes_consumed").glob("*.json")
    )
    shutil.rmtree(worker / ".trellis-history", ignore_errors=True)
