from pathlib import Path
import inspect
import os
import time

import trellis.atomic_actions.observations as observations
from trellis.atomic_actions.tablet_support import sync_tablet_support


def test_sync_tablet_support_writes_kernel_render_output(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    payload = sync_tablet_support(
        repo,
        {
            "index_md_path": str(repo / "Tablet" / "INDEX.md"),
            "index_md_content": "# kernel index\n",
            "readme_md_path": str(repo / "Tablet" / "README.md"),
            "readme_md_content": "# kernel readme\n",
            "header_tex_path": str(repo / "Tablet" / "header.tex"),
            "header_tex_content": "% kernel header\n",
        },
    )

    assert Path(payload["index_md_path"]).read_text(encoding="utf-8") == "# kernel index\n"
    assert Path(payload["readme_md_path"]).read_text(encoding="utf-8") == "# kernel readme\n"
    assert Path(payload["header_tex_path"]).read_text(encoding="utf-8") == "% kernel header\n"


def test_sync_tablet_support_preserves_existing_header_when_kernel_omits_content(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    tablet_dir = repo / "Tablet"
    tablet_dir.mkdir(parents=True)
    header = tablet_dir / "header.tex"
    header.write_text("% keep me\n", encoding="utf-8")

    payload = sync_tablet_support(
        repo,
        {
            "index_md_path": str(tablet_dir / "INDEX.md"),
            "index_md_content": "# index\n",
            "readme_md_path": str(tablet_dir / "README.md"),
            "readme_md_content": "# readme\n",
            "header_tex_path": str(header),
            "header_tex_content": None,
        },
    )

    assert Path(payload["index_md_path"]).read_text(encoding="utf-8") == "# index\n"
    assert Path(payload["readme_md_path"]).read_text(encoding="utf-8") == "# readme\n"
    assert header.read_text(encoding="utf-8") == "% keep me\n"
    assert str(header) not in payload["updated_paths"]


def test_prepare_compiled_support_runs_cache_get_only(
    tmp_path: Path,
    monkeypatch,
) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    calls: list[list[str]] = []

    def fake_run_lake_command(_repo: Path, args: list[str], *, timeout_secs: float, bwrap_role=None):
        calls.append(list(args))
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", fake_run_lake_command)

    # bwrap_role="lake_compiler" exercises the direct-lake path (the
    # server-side endpoint). With bwrap_role=None the checker socket is
    # mandatory and the call would raise; the direct path under test is
    # only reachable via an explicit role.
    payload = observations.prepare_compiled_support(repo, bwrap_role="lake_compiler")

    assert calls == [["exe", "cache", "get"]]
    assert payload["steps_completed"] == ["cache_get"]
    assert payload["returncode"] == 0


def test_lean_support_actions_share_long_default_timeout() -> None:
    for func_name in [
        "compile_node",
        "build_tablet",
        "prepare_compiled_support",
        "materialize_tablet_oleans",
        "print_axioms",
        "observe_lean_semantic_payloads",
    ]:
        func = getattr(observations, func_name)
        assert inspect.signature(func).parameters["timeout_secs"].default == observations.LEAN_SUPPORT_TIMEOUT_SECS


def test_run_lake_command_timeout_includes_command(monkeypatch, tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()

    def _fake_run(*args, **kwargs):
        raise observations.subprocess.TimeoutExpired(cmd=kwargs.get("args") or args[0], timeout=3600)

    monkeypatch.setattr(observations.subprocess, "run", _fake_run)

    payload = observations._run_lake_command(
        repo,
        ["env", "lean", "Tablet/Preamble.lean"],
        timeout_secs=3600.0,
    )

    assert payload["timed_out"] is True
    assert "lake env lean Tablet/Preamble.lean" in payload["stderr"]
    assert "3600.0s" in payload["stderr"]


def test_observe_lean_semantic_payloads_uses_explicit_script_path(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """Audit Fix 1 wiring: callers can pin the script path the lean process
    spawns. The default helper resolves to the trellis source-root copy,
    which the lake_compiler bwrap mounts read-only; an explicit override
    is the contract that future callers (e.g. a runtime-snapshot path) can
    rely on without touching observations.py.
    """
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    explicit_script = tmp_path / "alt_dir" / "lean_semantic_fingerprint.lean"
    explicit_script.parent.mkdir(parents=True)
    explicit_script.write_text("-- stub\n", encoding="utf-8")

    seen_args: list[list[str]] = []

    def _fake_run(_repo, args, *, timeout_secs, bwrap_role=None):
        seen_args.append(list(args))
        return {
            "returncode": 0,
            "stdout": f"FP\t{args[-1]}\tpayload-{args[-1]}\n",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", _fake_run)

    result = observations.observe_lean_semantic_payloads(
        repo,
        ["alpha"],
        bwrap_role="lake_compiler",
        script_path=explicit_script,
    )
    assert result == {"alpha": {"ok": True, "payload": "payload-alpha", "error": ""}}
    assert seen_args == [
        ["env", "lean", "--run", str(explicit_script), "alpha"],
    ]


def test_observe_lean_semantic_payloads_lake_compiler_default_path_is_under_bound_scripts_dir(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """Audit Fix 1 sanity: the default script path resolves under the
    trellis source ``scripts/`` directory, which sandbox.py mounts ro
    into the lake_compiler bwrap. Without this invariant the default
    code path silently 404s inside the bwrap.
    """
    from trellis import sandbox

    default_path = observations._lean_semantic_fingerprint_script_path()
    scripts_dir = sandbox._trellis_source_scripts_dir().resolve()
    assert default_path.resolve().is_relative_to(scripts_dir), (
        f"default script {default_path} must live under bound dir {scripts_dir}"
    )


# ----------------------------- materialize_tablet_oleans batched build -----------------------------


def _seed_tablet_node(repo: Path, node: str, body: str = "") -> None:
    """Create a minimal Tablet/<node>.lean source so materialization_order
    can walk it. Tests that drive materialize_tablet_oleans need real
    source files because the closure walk reads them off disk."""
    tablet = repo / "Tablet"
    tablet.mkdir(parents=True, exist_ok=True)
    (tablet / f"{node}.lean").write_text(body or "-- stub\n", encoding="utf-8")


def _write_olean(repo: Path, node: str, content: bytes = b"olean") -> Path:
    """Drop a stub olean blob at the supervisor's expected path. The
    materialize_tablet_oleans stat-walk only checks size>0 and mtime;
    contents are opaque."""
    olean = observations._tablet_olean_path(repo, node)
    olean.parent.mkdir(parents=True, exist_ok=True)
    olean.write_bytes(content)
    return olean


def test_materialize_tablet_oleans_uses_single_lake_build(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """Fix 2: materialize_tablet_oleans must invoke ``lake build`` exactly
    once with all targets, not once per node. Pin the batched call shape
    so a future regression to per-node loops is caught here.
    """
    repo = tmp_path / "repo"
    repo.mkdir()
    _seed_tablet_node(repo, "A")
    _seed_tablet_node(repo, "B")
    _seed_tablet_node(repo, "C")

    calls: list[list[str]] = []

    def fake_run_lake_command(_repo: Path, args, *, timeout_secs: float, bwrap_role=None):
        calls.append(list(args))
        # Materialize all requested oleans so the stat-walk reports them.
        for node in ("A", "B", "C"):
            _write_olean(repo, node)
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", fake_run_lake_command)

    payload = observations.materialize_tablet_oleans(
        repo, ["A", "B", "C"], bwrap_role="lake_compiler"
    )

    assert payload["returncode"] == 0
    assert sorted(payload["materialized_nodes"]) == ["A", "B", "C"]
    assert len(calls) == 1, f"expected 1 lake invocation, got {len(calls)}: {calls!r}"
    # First arg must be ``build``; targets must include every node.
    args = calls[0]
    assert args[0] == "build"
    targets = [a for a in args if a.startswith("Tablet.")]
    assert sorted(targets) == ["Tablet.A", "Tablet.B", "Tablet.C"]


def test_materialize_tablet_oleans_reports_partial_success_via_stat_check(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """Fix 2: even when lake exits nonzero (e.g. one node failed), the
    stat-walk must still surface every olean that landed before the
    failure point. The materialized_nodes set is determined from on-disk
    truth, not from lake's per-node parsed output.
    """
    repo = tmp_path / "repo"
    repo.mkdir()
    _seed_tablet_node(repo, "A")
    _seed_tablet_node(repo, "B")
    _seed_tablet_node(repo, "C")

    def fake_run_lake_command(_repo: Path, args, *, timeout_secs: float, bwrap_role=None):
        # Simulate partial success: A and B build, C fails.
        _write_olean(repo, "A")
        _write_olean(repo, "B")
        return {
            "returncode": 1,
            "stdout": "",
            "stderr": "Tablet/C.lean:1:0: error: stub failure\n",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", fake_run_lake_command)

    payload = observations.materialize_tablet_oleans(
        repo, ["A", "B", "C"], bwrap_role="lake_compiler"
    )

    assert payload["returncode"] == 1
    assert sorted(payload["materialized_nodes"]) == ["A", "B"], (
        f"stat-walk must surface oleans that landed pre-failure, got {payload['materialized_nodes']!r}"
    )


def test_materialize_tablet_oleans_includes_already_current_closure_deps(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """M1: ``materialized_nodes`` must include every closure node whose
    olean is current (size>0, mtime>=source mtime), not just nodes whose
    oleans were freshly written by *this* lake invocation. Lake skips
    already-current targets (it doesn't touch their oleans), so a
    build-window mtime gate would wrongly exclude unchanged dependencies
    from the contract surface.
    """
    repo = tmp_path / "repo"
    repo.mkdir()
    # A imports B; closure walked by materialization_order is [B, A].
    _seed_tablet_node(repo, "B", body="-- B\n")
    _seed_tablet_node(repo, "A", body="import Tablet.B\n")

    # Pre-build B's olean with mtime safely in the past (older than any
    # wallclock the test could observe later) but still >= B.lean's mtime
    # so the source-mtime gate considers it current. Lake will not touch
    # this olean during the simulated build.
    olean_b = _write_olean(repo, "B")
    src_b = repo / "Tablet" / "B.lean"
    far_past = time.time() - 3600.0
    os.utime(src_b, (far_past, far_past))
    os.utime(olean_b, (far_past, far_past))

    def fake_run_lake_command(_repo: Path, args, *, timeout_secs: float, bwrap_role=None):
        # Simulate lake building only A; B's olean is left untouched
        # because it was already current.
        _write_olean(repo, "A")
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", fake_run_lake_command)

    payload = observations.materialize_tablet_oleans(
        repo, ["A"], bwrap_role="lake_compiler"
    )

    assert payload["returncode"] == 0
    assert sorted(payload["materialized_nodes"]) == ["A", "B"], (
        "closure deps with current oleans must appear in materialized_nodes "
        f"even when lake didn't rewrite them; got {payload['materialized_nodes']!r}"
    )


