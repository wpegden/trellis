from pathlib import Path
import inspect
import json
import os
import time

import pytest

import trellis.atomic_actions.observations as observations
from trellis.atomic_actions.tablet_support import sync_tablet_support


_REAL_KERNEL_REPLAY_TABLET_MODULES = observations._kernel_replay_tablet_modules
_MANIFEST_SCRIPT = (
    Path(observations.__file__).resolve().parents[2]
    / "scripts"
    / "lean_module_manifest.lean"
)


def _manifest_stdout_for_args(args: list[str]) -> str:
    if "lean_module_manifest.lean" not in " ".join(args):
        return ""
    nodes = args[args.index("--run") + 2 :]
    return "\n".join(
        json.dumps(
            {
                "node": node,
                "declarations": [{"name": node, "kind": "theorem"}],
                "ownership_manifest": [{"name": node, "kind": "theorem"}],
                "artifact_parts": [
                    {
                        "level": "exported",
                        "path": f"/fake/Tablet/{node}.olean",
                        "declarations": [{"name": node, "kind": "theorem"}],
                        "extra_const_names": [],
                    }
                ],
                "direct_imports": [
                    {
                        "module": "Init",
                        "olean": "/pinned-lean/lib/lean/Init.olean",
                    }
                ],
                "sysroot": "/pinned-lean",
            }
        )
        for node in nodes
    )


@pytest.fixture(autouse=True)
def _stub_kernel_replay_for_observation_units(monkeypatch) -> None:
    """Keep observation unit tests hermetic; dedicated tests exercise replay."""

    def replay_ok(_repo, node_names, *, timeout_secs, bwrap_role):
        return {
            "checked_nodes": list(node_names),
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_kernel_replay_tablet_modules", replay_ok)


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


def test_sync_tablet_support_is_a_readonly_noop_when_content_current(
    tmp_path: Path,
) -> None:
    """EROFS regression guard: non-worker roles run with Tablet/ ro-bound,
    where even mkstemp fails. When the rendered content matches disk the
    sync must not attempt any write (chmod-readonly dir stands in for the
    ro bwrap mount); when content differs it must still fail loudly."""
    repo = tmp_path / "repo"
    tablet_dir = repo / "Tablet"
    tablet_dir.mkdir(parents=True)
    (tablet_dir / "INDEX.md").write_text("# index\n", encoding="utf-8")
    (tablet_dir / "README.md").write_text("# readme\n", encoding="utf-8")
    (tablet_dir / "header.tex").write_text("% header\n", encoding="utf-8")
    render = {
        "index_md_path": str(tablet_dir / "INDEX.md"),
        "index_md_content": "# index\n",
        "readme_md_path": str(tablet_dir / "README.md"),
        "readme_md_content": "# readme\n",
        "header_tex_path": str(tablet_dir / "header.tex"),
        "header_tex_content": "% header\n",
    }

    os.chmod(tablet_dir, 0o555)
    try:
        payload = sync_tablet_support(repo, render)
        # Observable output is unchanged by the skip: all managed paths
        # are still reported.
        assert payload["updated_paths"] == [
            str(tablet_dir / "INDEX.md"),
            str(tablet_dir / "README.md"),
            str(tablet_dir / "header.tex"),
        ]

        stale = dict(render, index_md_content="# newer index\n")
        with pytest.raises(OSError):
            sync_tablet_support(repo, stale)
    finally:
        os.chmod(tablet_dir, 0o755)


def test_prepare_compiled_support_runs_cache_get_only(
    tmp_path: Path,
    monkeypatch,
) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    calls: list[list[str]] = []

    def fake_run_lake_command(_repo: Path, args: list[str], *, timeout_secs: float, bwrap_role=None, metrics=None):
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

    # `_run_lake_command` drives `_Wait4Popen` (Popen + os.wait4 for rusage)
    # rather than `subprocess.run`, so the timeout is raised out of
    # `communicate` — the same place the stdlib raises it.
    class _TimingOutPopen:
        args = ["lake"]
        # Never a live process: the sampler walks /proc/-1, finds nothing,
        # and must report no measurement.
        pid = -1

        def __init__(self, *args, **kwargs):
            self.child_rusage = None

        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

        def communicate(self, timeout=None):
            raise observations.subprocess.TimeoutExpired(cmd=self.args, timeout=timeout)

        def kill(self):
            pass

        def wait(self):
            return -9

    monkeypatch.setattr(observations, "_Wait4Popen", _TimingOutPopen)

    metrics: dict[str, object] = {}
    payload = observations._run_lake_command(
        repo,
        ["env", "lean", "Tablet/Preamble.lean"],
        timeout_secs=3600.0,
        metrics=metrics,
    )

    assert payload["timed_out"] is True
    assert "lake env lean Tablet/Preamble.lean" in payload["stderr"]
    assert "3600.0s" in payload["stderr"]
    assert metrics == {}, "a timed-out build must record no measurement"


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

    def _fake_run(_repo, args, *, timeout_secs, bwrap_role=None, metrics=None):
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
        ["env", "lean", "--run", str(explicit_script), "alpha", "alpha"],
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
    materialize_tablet_oleans content-walk checks size>0 and a provenance
    sidecar matching the current source-closure hash; contents are opaque."""
    olean = observations._tablet_olean_path(repo, node)
    olean.parent.mkdir(parents=True, exist_ok=True)
    olean.write_bytes(content)
    return olean


def _seed_closure_base(repo: Path) -> None:
    """Seed the files ``tablet_source_closure_hash`` needs to compute a hash
    (``.trellis/scripts/check.py`` + ``Tablet/Preamble.lean``). Without these
    the source-closure hash is ``None`` (fail-closed) and no olean is ever
    treated current — the content-freshness contract introduced by the
    olean-staleness fix."""
    (repo / ".trellis" / "scripts").mkdir(parents=True, exist_ok=True)
    (repo / ".trellis" / "scripts" / "check.py").write_text(
        "#!/usr/bin/env python3\n", encoding="utf-8"
    )
    (repo / "Tablet").mkdir(parents=True, exist_ok=True)
    (repo / "Tablet" / "Preamble.lean").write_text(
        "import Mathlib.Data.Nat.Basic\n", encoding="utf-8"
    )


def _seed_olean_provenance(repo: Path, node: str) -> None:
    """Record a provenance sidecar marking ``node``'s olean current with its
    current source (what the materialize post-build walk would write)."""
    observations.write_olean_srcclosure(
        repo, node, observations.tablet_source_closure_hash(repo, node)
    )


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
    _seed_closure_base(repo)
    _seed_tablet_node(repo, "A")
    _seed_tablet_node(repo, "B")
    _seed_tablet_node(repo, "C")

    calls: list[list[str]] = []

    def fake_run_lake_command(_repo: Path, args, *, timeout_secs: float, bwrap_role=None, metrics=None):
        calls.append(list(args))
        # Materialize all requested oleans so the content-walk reports them.
        for node in ("A", "B", "C"):
            _write_olean(repo, node)
        return {
            "returncode": 0,
            "stdout": _manifest_stdout_for_args(list(args)),
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


def test_kernel_replay_uses_exact_tablet_modules(
    tmp_path: Path,
    monkeypatch,
) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    _write_olean(repo, "Dep", b"dep-olean")
    _write_olean(repo, "Root", b"root-olean")
    calls: list[tuple[list[str], str | None]] = []

    def fake_run(_repo, args, *, timeout_secs, bwrap_role=None, metrics=None):
        calls.append((list(args), bwrap_role))
        return {
            "returncode": 0,
            "stdout": _manifest_stdout_for_args(list(args)),
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", fake_run)
    payload = _REAL_KERNEL_REPLAY_TABLET_MODULES(
        repo,
        ["Dep", "Root", "Dep", ""],
        timeout_secs=17.0,
        bwrap_role="lake_compiler",
    )

    assert calls == [
        (["env", "leanchecker", "Tablet.Dep", "Tablet.Root"], "lake_compiler"),
        (
            [
                "env",
                "lean",
                "--run",
                    str(_MANIFEST_SCRIPT),
                "Dep",
                "Root",
            ],
            "lake_compiler",
        ),
    ]
    assert payload["checked_nodes"] == ["Dep", "Root"]
    assert payload["reused_nodes"] == []
    assert payload["attested_nodes"] == ["Dep", "Root"]
    assert observations.olean_has_current_kernel_replay(repo, "Dep")
    assert observations.olean_has_current_kernel_replay(repo, "Root")


def test_replay_import_boundary_accepts_pinned_and_project_modules(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    project_lib = repo / ".lake" / "build" / "lib" / "lean"
    package_lib = repo / ".lake" / "packages" / "mathlib" / ".lake" / "build" / "lib" / "lean"
    sysroot = tmp_path / "lean"
    for path in (project_lib / "Tablet", package_lib / "Mathlib", sysroot / "lib" / "lean"):
        path.mkdir(parents=True, exist_ok=True)
    (repo / "lake-manifest.json").write_text(
        json.dumps({"packagesDir": ".lake/packages", "packages": []}),
        encoding="utf-8",
    )
    rows = {
        "A": [
            {
                "module": "Tablet.Helper",
                "olean": str(project_lib / "Tablet" / "Helper.olean"),
                "sysroot": str(sysroot),
            },
            {
                "module": "Mathlib.Data.Nat.Basic",
                "olean": str(package_lib / "Mathlib" / "Data.Nat.Basic.olean"),
                "sysroot": str(sysroot),
            },
            {
                "module": "Init",
                "olean": str(sysroot / "lib" / "lean" / "Init.olean"),
                "sysroot": str(sysroot),
            },
        ]
    }

    trusted, errors = observations._validate_replay_import_boundaries(
        repo, ["A"], rows
    )

    assert errors == []
    assert [entry["module"] for entry in trusted["A"]] == [
        "Init",
        "Mathlib.Data.Nat.Basic",
        "Tablet.Helper",
    ]


def test_replay_import_boundary_rejects_unmanaged_module(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    sysroot = tmp_path / "lean"
    unmanaged = tmp_path / "unmanaged" / "Outside.olean"
    rows = {
        "A": [
            {
                "module": "Outside",
                "olean": str(unmanaged),
                "sysroot": str(sysroot),
            }
        ]
    }

    trusted, errors = observations._validate_replay_import_boundaries(
        repo, ["A"], rows
    )

    assert trusted["A"] == []
    assert len(errors) == 1
    assert "Outside" in errors[0]
    assert str(unmanaged) in errors[0]
    assert "pinned Lake dependency" in errors[0]


def test_kernel_replay_reuses_attested_bytes_and_checks_only_changed_olean(
    tmp_path: Path,
    monkeypatch,
) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    _write_olean(repo, "A", b"olean-a-v1")
    _write_olean(repo, "B", b"olean-b-v1")
    calls: list[list[str]] = []

    def fake_run(_repo, args, *, timeout_secs, bwrap_role=None, metrics=None):
        calls.append(list(args))
        return {
            "returncode": 0,
            "stdout": _manifest_stdout_for_args(list(args)),
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", fake_run)
    cold = _REAL_KERNEL_REPLAY_TABLET_MODULES(
        repo, ["A", "B"], timeout_secs=17.0, bwrap_role="lake_compiler"
    )
    assert cold["checked_nodes"] == ["A", "B"]
    assert calls == [
        ["env", "leanchecker", "Tablet.A", "Tablet.B"],
        [
            "env",
            "lean",
            "--run",
            str(_MANIFEST_SCRIPT),
            "A",
            "B",
        ],
    ]

    calls.clear()
    warm = _REAL_KERNEL_REPLAY_TABLET_MODULES(
        repo, ["A", "B"], timeout_secs=17.0, bwrap_role="lake_compiler"
    )
    assert warm["checked_nodes"] == []
    assert warm["reused_nodes"] == ["A", "B"]
    assert calls == [], "settled modules must cost no leanchecker process"

    _write_olean(repo, "B", b"olean-b-v2")
    changed = _REAL_KERNEL_REPLAY_TABLET_MODULES(
        repo, ["A", "B"], timeout_secs=17.0, bwrap_role="lake_compiler"
    )
    assert changed["checked_nodes"] == ["B"]
    assert changed["reused_nodes"] == ["A"]
    assert calls == [
        ["env", "leanchecker", "Tablet.B"],
        [
            "env",
            "lean",
            "--run",
            str(_MANIFEST_SCRIPT),
            "B",
        ],
    ]


def test_kernel_replay_attestation_invalidates_with_toolchain_pin(
    tmp_path: Path,
    monkeypatch,
) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    (repo / "lean-toolchain").write_text("leanprover/lean4:v4.29.0\n")
    _write_olean(repo, "A", b"olean-a")
    calls: list[list[str]] = []

    def fake_run(_repo, args, *, timeout_secs, bwrap_role=None, metrics=None):
        calls.append(list(args))
        return {
            "returncode": 0,
            "stdout": _manifest_stdout_for_args(list(args)),
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", fake_run)
    _REAL_KERNEL_REPLAY_TABLET_MODULES(
        repo, ["A"], timeout_secs=17.0, bwrap_role="lake_compiler"
    )
    assert observations.olean_has_current_kernel_replay(repo, "A")

    (repo / "lean-toolchain").write_text("leanprover/lean4:v4.30.0\n")
    assert not observations.olean_has_current_kernel_replay(repo, "A")
    _REAL_KERNEL_REPLAY_TABLET_MODULES(
        repo, ["A"], timeout_secs=17.0, bwrap_role="lake_compiler"
    )
    assert calls == [
        ["env", "leanchecker", "Tablet.A"],
        [
            "env",
            "lean",
            "--run",
                str(_MANIFEST_SCRIPT),
            "A",
        ],
        ["env", "leanchecker", "Tablet.A"],
        [
            "env",
            "lean",
            "--run",
                str(_MANIFEST_SCRIPT),
            "A",
        ],
    ]


def test_full_tablet_build_purges_source_stale_attested_olean(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """The full-build call site must not reuse replay state after source drift."""
    repo = tmp_path / "repo"
    repo.mkdir()
    _seed_closure_base(repo)
    _seed_tablet_node(repo, "A")
    _write_olean(repo, "A", b"olean-a-old")
    old_source_hash = observations.tablet_source_closure_hash(repo, "A")
    assert old_source_hash is not None
    observations.write_olean_srcclosure(repo, "A", old_source_hash)
    old_olean_hash = observations._olean_sha256(repo, "A")
    assert old_olean_hash is not None
    assert observations._write_kernel_replay_attestation(
        repo, "A", olean_sha256=old_olean_hash
    )

    (repo / "Tablet" / "A.lean").write_text(
        "import Tablet.Preamble\ntheorem A : True := by trivial\n",
        encoding="utf-8",
    )

    def fake_build(_repo, args, *, timeout_secs, bwrap_role=None, metrics=None):
        assert args == ["build", "Tablet"]
        assert not observations._tablet_olean_path(repo, "A").exists()
        assert observations.read_olean_srcclosure(repo, "A") is None
        _write_olean(repo, "A", b"olean-a-rebuilt")
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", fake_build)
    payload = observations.build_tablet(repo, bwrap_role="lake_compiler")

    assert payload["returncode"] == 0
    assert observations._tablet_olean_path(repo, "A").read_bytes() == b"olean-a-rebuilt"


def test_materialize_rejects_and_purges_kernel_invalid_olean(
    tmp_path: Path,
    monkeypatch,
) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    _seed_closure_base(repo)
    _seed_tablet_node(repo, "IllTypedSkip")

    def fake_build(_repo, args, *, timeout_secs, bwrap_role=None, metrics=None):
        _write_olean(repo, "IllTypedSkip", b"unchecked")
        return {
            "returncode": 0,
            "stdout": "build succeeded\n",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    def fake_replay(_repo, node_names, *, timeout_secs, bwrap_role):
        assert list(node_names) == ["IllTypedSkip"]
        return {
            "checked_nodes": ["IllTypedSkip"],
            "returncode": 1,
            "stdout": "",
            "stderr": "declaration type mismatch: expected False, got True\n",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", fake_build)
    monkeypatch.setattr(observations, "_kernel_replay_tablet_modules", fake_replay)

    payload = observations.materialize_tablet_oleans(
        repo, ["IllTypedSkip"], bwrap_role="lake_compiler"
    )

    assert payload["returncode"] == 1
    assert payload["materialized_nodes"] == []
    assert "[kernel_replay]" in payload["stderr"]
    assert "declaration type mismatch" in payload["stderr"]
    assert not observations._tablet_olean_path(repo, "IllTypedSkip").exists()
    assert observations.read_olean_srcclosure(repo, "IllTypedSkip") is None


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
    _seed_closure_base(repo)
    _seed_tablet_node(repo, "A")
    _seed_tablet_node(repo, "B")
    _seed_tablet_node(repo, "C")

    def fake_run_lake_command(_repo: Path, args, *, timeout_secs: float, bwrap_role=None, metrics=None):
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
    _seed_closure_base(repo)
    # A imports B; closure walked by materialization_order is [B, A].
    _seed_tablet_node(repo, "B", body="-- B\n")
    _seed_tablet_node(repo, "A", body="import Tablet.B\n")

    # Pre-build B's olean and record its provenance so the content gate
    # treats it as already-current. mtime is irrelevant under the
    # content-freshness contract; the far-past stamp is retained only to
    # prove the decision no longer depends on it. Lake will not touch this
    # olean during the simulated build.
    olean_b = _write_olean(repo, "B")
    _seed_olean_provenance(repo, "B")
    src_b = repo / "Tablet" / "B.lean"
    far_past = time.time() - 3600.0
    os.utime(src_b, (far_past, far_past))
    os.utime(olean_b, (far_past, far_past))

    def fake_run_lake_command(_repo: Path, args, *, timeout_secs: float, bwrap_role=None, metrics=None):
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




# --------------------- elaboration-cost measurement (Stage 0) ---------------------


def _fake_lake_with_metrics(
    repo: Path, built: tuple[str, ...], *, peak_rss_kib: int = 2_400_000
):
    """Build a `_run_lake_command` stand-in that materializes ``built`` and
    fills the ``metrics`` out-dict the way a real completed lake run does.
    The default peak sits above `_ELABORATION_PEAK_RSS_FLOOR_KIB`, as any
    real elaboration's does."""

    def _fake(_repo: Path, args, *, timeout_secs: float, bwrap_role=None, metrics=None):
        for node in built:
            _write_olean(repo, node, b"olean-bytes")
        if metrics is not None:
            metrics["walltime_ms"] = 1234
            metrics["peak_rss_kib"] = peak_rss_kib
            metrics["peak_rss_process"] = "lean"
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    return _fake


def test_elaboration_cost_attributed_only_to_a_single_stale_node(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """The attribution rule: a cost record is emitted iff the lake
    invocation's stale set is exactly one node.

    Three cases in one test because they are the same rule read three ways —
    one stale node attributes, two stale nodes cannot be told apart inside a
    single lake process, and a fully-current closure elaborated nothing at
    all. Getting any of them wrong fabricates a measurement.
    """
    repo = tmp_path / "repo"
    repo.mkdir()
    _seed_closure_base(repo)
    _seed_tablet_node(repo, "A")
    _seed_tablet_node(repo, "B")

    # (1) Stale set == {A}: B is already content-current, A has no olean.
    _write_olean(repo, "B")
    _seed_olean_provenance(repo, "B")
    monkeypatch.setattr(
        observations, "_run_lake_command", _fake_lake_with_metrics(repo, ("A",))
    )
    payload = observations.materialize_tablet_oleans(
        repo, ["A", "B"], bwrap_role="lake_compiler"
    )
    cost = payload.get("cost")
    assert cost is not None, "a single-node stale set must produce a measurement"
    assert payload["stale_node_count"] == 1
    assert cost["node"] == "A"
    assert cost["walltime_ms"] == 1234
    assert cost["peak_rss_kib"] == 2_400_000
    assert cost["peak_rss_process"] == "lean"
    assert cost["olean_size_bytes"] == len(b"olean-bytes")
    assert cost["source_closure_hash"] == observations.tablet_source_closure_hash(
        repo, "A"
    )

    # (2) Fully-current closure: nothing was elaborated, nothing is recorded.
    _seed_olean_provenance(repo, "A")
    monkeypatch.setattr(
        observations, "_run_lake_command", _fake_lake_with_metrics(repo, ())
    )
    payload = observations.materialize_tablet_oleans(
        repo, ["A", "B"], bwrap_role="lake_compiler"
    )
    assert "cost" not in payload, "a build with an empty stale set must record nothing"
    assert payload["stale_node_count"] == 0

    # (3) Stale set == {A, B}: one lake process, no honest per-node split.
    observations._purge_olean_artifacts(repo, "A")
    observations._purge_olean_artifacts(repo, "B")
    monkeypatch.setattr(
        observations, "_run_lake_command", _fake_lake_with_metrics(repo, ("A", "B"))
    )
    payload = observations.materialize_tablet_oleans(
        repo, ["A", "B"], bwrap_role="lake_compiler"
    )
    assert "cost" not in payload, "a multi-node stale set must record nothing"
    # The count is what makes the rule checkable from the request log: a
    # logged line pairing a cost with a count above 1 is a broken gate.
    assert payload["stale_node_count"] == 2


def test_elaboration_cost_heartbeats_ride_the_side_file(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """Heartbeat side-channel, pinned at both seams.

    Seam 1 — the feeding seam: a real ``_run_lake_command`` exports
    ``TRELLIS_HB_OUT`` to the child (a fake ``lake`` on PATH stands in for
    the plugin) and parses whatever the child appended: the pinned wire
    format with extra keys ignored, malformed / partial / negative /
    non-integer lines skipped, last line wins, and the side file removed
    afterwards. A metrics-less invocation exports nothing.

    Seam 2 — the payload: a count for the attributed module lands on the
    cost dict as ``heartbeats``; a build with no side-channel data yields
    the same cost dict with the other three fields intact and no
    ``heartbeats`` key.
    """
    repo = tmp_path / "repo"
    repo.mkdir()
    _seed_closure_base(repo)
    _seed_tablet_node(repo, "A")

    # ---- Seam 1: env injection, fail-open parse, cleanup.
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    witness = tmp_path / "hb-env-witness"
    fake_lake = bin_dir / "lake"
    fake_lake.write_text(
        "#!/bin/sh\n"
        'printf \'%s\' "${TRELLIS_HB_OUT:-}" > "$FAKE_HB_WITNESS"\n'
        'if [ -n "${TRELLIS_HB_OUT:-}" ]; then\n'
        "  {\n"
        "    printf '%s\\n' '{\"module\": \"Tablet.A\", \"decl\": \"helper\", \"heartbeats\": 500, \"extra\": \"ignored\"}'\n"
        "    printf '%s\\n' 'not json at all'\n"
        "    printf '%s\\n' '{\"module\": \"Tablet.A\", \"heartbeats\": -5}'\n"
        "    printf '%s\\n' '{\"module\": \"Tablet.B\", \"heartbeats\": \"many\"}'\n"
        "    printf '%s\\n' '{\"module\": 3, \"heartbeats\": 9}'\n"
        "    printf '%s\\n' '{\"module\": \"Tablet.A\", \"decl\": \"A\", \"heartbeats\": 777}'\n"
        "    printf '{\"module\": \"Tablet.A\", \"hea'\n"
        '  } >> "$TRELLIS_HB_OUT"\n'
        "fi\n"
        "exit 0\n",
        encoding="utf-8",
    )
    fake_lake.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("FAKE_HB_WITNESS", str(witness))

    metrics: dict = {}
    payload = observations._run_lake_command(
        repo, ["build", "Tablet.A"], timeout_secs=30.0, metrics=metrics
    )
    assert payload["returncode"] == 0
    assert metrics["heartbeats_by_module"] == {"Tablet.A": 777}, (
        "extra keys ignored, malformed/partial/negative/non-integer lines "
        f"skipped, last line wins; got {metrics.get('heartbeats_by_module')!r}"
    )
    assert list((repo / ".lake" / "build").glob("trellis-hb.*.jsonl")) == [], (
        "the per-invocation side file must be removed after parsing"
    )
    # A metrics-less caller (read-only olean consumer) exports no side-file
    # path at all — the child must see the env var unset.
    payload = observations._run_lake_command(repo, ["build"], timeout_secs=30.0)
    assert payload["returncode"] == 0
    assert witness.read_text(encoding="utf-8") == ""

    # ---- Seam 2: the count reaches the cost dict; its absence costs nothing.
    def _fake_lake(with_heartbeats: bool):
        def _fake(_repo, args, *, timeout_secs, bwrap_role=None, metrics=None):
            _write_olean(repo, "A", b"olean-bytes")
            if metrics is not None:
                metrics["walltime_ms"] = 1234
                metrics["peak_rss_kib"] = 2_400_000
                metrics["peak_rss_process"] = "lean"
                if with_heartbeats:
                    metrics["heartbeats_by_module"] = {"Tablet.A": 777}
            return {
                "returncode": 0,
                "stdout": "",
                "stderr": "",
                "timed_out": False,
                "spawn_error": "",
            }

        return _fake

    monkeypatch.setattr(observations, "_run_lake_command", _fake_lake(True))
    payload = observations.materialize_tablet_oleans(
        repo, ["A"], bwrap_role="lake_compiler"
    )
    assert payload["cost"]["heartbeats"] == 777

    # No side-channel data (absent or wholly malformed side file): the other
    # three fields must be recorded exactly as before, without a heartbeats
    # key — instrumentation absence never degrades the measurement.
    observations._purge_olean_artifacts(repo, "A")
    monkeypatch.setattr(observations, "_run_lake_command", _fake_lake(False))
    payload = observations.materialize_tablet_oleans(
        repo, ["A"], bwrap_role="lake_compiler"
    )
    cost = payload["cost"]
    assert "heartbeats" not in cost
    assert cost["walltime_ms"] == 1234
    assert cost["peak_rss_kib"] == 2_400_000
    assert cost["olean_size_bytes"] == len(b"olean-bytes")


def test_elaboration_cost_floor_drops_subfloor_measurements(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """A peak below the sampling floor is a failed sampling, not a small
    build: no record, and a visible `cost_floor_violation` marker.

    This is the check that would have caught the `wait4` echo bug on its
    first record — the echoed value used here, 48,748 KiB, is the exact one
    observed live, against a 256 MiB floor.
    """
    repo = tmp_path / "repo"
    repo.mkdir()
    _seed_closure_base(repo)
    _seed_tablet_node(repo, "A")

    monkeypatch.setattr(
        observations,
        "_run_lake_command",
        _fake_lake_with_metrics(repo, ("A",), peak_rss_kib=48_748),
    )
    payload = observations.materialize_tablet_oleans(
        repo, ["A"], bwrap_role="lake_compiler"
    )
    assert "cost" not in payload, "a sub-floor measurement must not become a record"
    violation = payload["cost_floor_violation"]
    assert violation["node"] == "A"
    assert violation["peak_rss_kib"] == 48_748
    assert violation["floor_kib"] == observations._ELABORATION_PEAK_RSS_FLOOR_KIB
    # The marker must survive the chunk merge the same way `cost` does, so a
    # chunked request still shows the drop in the request log.
    merged = observations._merge_materialize_chunks([dict(payload)])
    assert merged["cost_floor_violation"] == violation

    # The floor must stay BELOW the real zero-import `lean` peak, or it stops
    # catching failed samplings and starts discarding valid measurements —
    # the inverse of its purpose. 418,820 KiB is the measured peak of `lean`
    # on an empty module (toolchain v4.30.0-rc1); the research report's
    # 440 MiB would have sat above it.
    measured_empty_module_lean_kib = 418_820
    assert observations._ELABORATION_PEAK_RSS_FLOOR_KIB < measured_empty_module_lean_kib, (
        "the floor must not exceed the smallest real elaboration; raising it "
        "above the zero-import lean peak would drop valid records"
    )


# A stand-in `lake` for the sampler tests: bash spawns a python GRANDCHILD
# (the production shape is `bwrap -> lake -> lean`, so the peak must be found
# by walking the tree, never by looking at the direct child), which allocates
# `$1` MiB, touches every page so the allocation is resident rather than
# lazily zero-mapped, and HOLDS it for a few sampler poll intervals.
_HOG_LAKE_SCRIPT = """#!/bin/bash
python3 - "$1" <<'EOF'
import sys, time
mib = int(sys.argv[1])
b = bytearray(mib * 1024 * 1024)
for i in range(0, len(b), 4096):
    b[i] = 1
time.sleep(0.8)
EOF
"""


def _write_hog_lake(bin_dir: Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    lake = bin_dir / "lake"
    lake.write_text(_HOG_LAKE_SCRIPT, encoding="utf-8")
    lake.chmod(0o755)


def test_lake_compiler_cannot_rewrite_tablet_source(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """The authoritative elaborator cannot mutate the source it certifies."""
    from trellis.sandbox import bwrap_available

    if not bwrap_available():
        pytest.skip("bwrap not installed")

    fake_elan = tmp_path / "elan"
    fake_bin = fake_elan / "bin"
    fake_bin.mkdir(parents=True)
    lake = fake_bin / "lake"
    lake.write_text(
        "#!/usr/bin/env python3\n"
        "from pathlib import Path\n"
        "Path('Tablet/Victim.lean').write_text('mutated\\n', encoding='utf-8')\n",
        encoding="utf-8",
    )
    lake.chmod(0o755)
    monkeypatch.setenv("ELAN_HOME", str(fake_elan))

    workspace = tmp_path / "ws"
    repo = workspace / "repo"
    tablet = repo / "Tablet"
    tablet.mkdir(parents=True)
    (workspace / "home").mkdir()
    victim = tablet / "Victim.lean"
    victim.write_text("original\n", encoding="utf-8")

    payload = observations._run_lake_command(
        repo,
        [],
        timeout_secs=30.0,
        bwrap_role="lake_compiler",
    )

    assert payload["returncode"] != 0
    assert victim.read_text(encoding="utf-8") == "original\n"


def test_run_lake_command_peak_rss_is_sampled_per_invocation(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """`peak_rss_kib` is the sampled tree peak, scoped to one invocation.

    Two properties in one drive because they are the same measurement read
    twice: a big run's grandchild allocation must be visible (the sampler
    walked the tree and read the right process), and a following small run
    must read small (each invocation gets a fresh sampler — the
    `RUSAGE_CHILDREN` trap, a high-water mark over every child ever reaped,
    would make the second reading >= the first forever).
    """
    _write_hog_lake(tmp_path / "bin")
    monkeypatch.setenv("PATH", f"{tmp_path / 'bin'}{os.pathsep}{os.environ['PATH']}")

    repo = tmp_path / "repo"
    repo.mkdir()

    def _metrics(alloc_mib: int) -> dict:
        metrics: dict = {}
        payload = observations._run_lake_command(
            repo, [str(alloc_mib)], timeout_secs=120.0, metrics=metrics
        )
        assert payload["returncode"] == 0, payload["stderr"]
        assert "walltime_ms" in metrics
        assert "peak_rss_kib" in metrics, "the sampler must have observed the tree"
        return metrics

    big = _metrics(256)
    small = _metrics(1)
    assert int(big["peak_rss_kib"]) > 200 * 1024, (
        f"a 256 MiB grandchild must be visible to the sampler; got "
        f"{big['peak_rss_kib']} KiB. A near-zero reading means the tree walk "
        "stopped at the direct child"
    )
    assert big["peak_rss_process"].startswith("python"), (
        f"the maximum must be attributed to the allocating process, got "
        f"{big['peak_rss_process']!r}"
    )
    assert int(small["peak_rss_kib"]) < int(big["peak_rss_kib"]), (
        "the peak must be scoped to the single invocation; a monotone "
        f"non-decreasing reading (big={big['peak_rss_kib']} KiB, "
        f"small={small['peak_rss_kib']} KiB) means state is leaking across "
        "sampler instances"
    )


def test_peak_rss_sampler_sees_through_production_bwrap(
    tmp_path: Path,
    monkeypatch,
) -> None:
    """The sampler measures a hog behind the REAL production sandbox flags,
    where `os.wait4` demonstrably cannot.

    This is the regression test for the `--unshare-pid` hole (see
    `_HostProcPeakSampler`): bubblewrap's monitor exits without reaping the
    pid-namespace init that carries the build tree's folded `cmaxrss`, so
    `wait4` returns the monitor's copy-on-write echo of THIS process's RSS.
    The earlier verification missed the bug precisely by testing without
    `--unshare-pid`, so this drive goes through `wrap_command`'s
    `lake_compiler` role — the exact confinement the live checker uses —
    with the `lake` on PATH replaced by a memory hog (never a real build:
    the flags are what is under test, not lean).
    """
    from trellis.sandbox import bwrap_available

    if not bwrap_available():
        pytest.skip("bwrap not installed")

    # `_run_lake_command(bwrap_role=...)` resolves the outer `lake` by
    # prepending `worker_elan_home()/bin` to PATH, and `wrap_command`
    # ro-binds that elan home into the sandbox. Pointing ELAN_HOME at a
    # synthetic tree whose bin/ holds the hog therefore both routes the
    # spawn to the hog and makes it visible inside bwrap.
    fake_elan = tmp_path / "elan"
    _write_hog_lake(fake_elan / "bin")
    monkeypatch.setenv("ELAN_HOME", str(fake_elan))

    workspace = tmp_path / "ws"
    repo = workspace / "repo"
    repo.mkdir(parents=True)
    (workspace / "home").mkdir()

    with open("/proc/self/status") as handle:
        self_rss_kib = next(
            int(line.split()[1]) for line in handle if line.startswith("VmRSS:")
        )

    hog_mib = 300
    metrics: dict = {}
    payload = observations._run_lake_command(
        repo,
        [str(hog_mib)],
        timeout_secs=120.0,
        bwrap_role="lake_compiler",
        metrics=metrics,
    )
    assert payload["returncode"] == 0, payload["stderr"]
    assert payload["spawn_error"] == "", payload["spawn_error"]

    peak_kib = int(metrics["peak_rss_kib"])
    assert peak_kib >= hog_mib * 1024, (
        f"the sampler must see the {hog_mib} MiB hog through the pid "
        f"namespace; got {peak_kib} KiB"
    )
    assert peak_kib < (hog_mib + 200) * 1024, (
        f"sampled peak {peak_kib} KiB is implausibly larger than the hog — "
        "the walk escaped the spawned tree"
    )
    assert metrics["peak_rss_process"].startswith("python"), metrics
    # The anti-echo half: under these flags `wait4` must NOT see the hog —
    # its reading is bounded by the CoW echo of this process's own RSS. If
    # this ever fails with a hog-sized value, bubblewrap started reaping its
    # namespace init and the diagnostic could be promoted back to a
    # measurement.
    wait4_kib = int(metrics["wait4_maxrss_kib"])
    assert wait4_kib < peak_kib, "wait4 saw the hog; see comment above"
    assert wait4_kib <= self_rss_kib + 100 * 1024, (
        f"wait4 reading {wait4_kib} KiB is neither the hog nor an echo of "
        f"this process ({self_rss_kib} KiB) — the mechanism changed"
    )


def test_peak_rss_sampler_stops_cleanly_and_skips_vanished_pids() -> None:
    """The sampler thread must terminate on stop() on every path, and a walk
    over a pid tree that no longer exists must degrade to "no measurement"
    rather than raising into the thread."""
    import subprocess as sp
    import sys

    # Live process, killed mid-sampling: stop() must join promptly.
    proc = sp.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
    sampler = observations._HostProcPeakSampler(proc.pid)
    sampler.start()
    time.sleep(0.25)
    proc.kill()
    proc.wait()
    peak_kib, name = sampler.stop()
    assert not sampler._thread.is_alive(), "sampler thread leaked past stop()"
    assert peak_kib is not None and peak_kib > 0, (
        "a live python process held for two poll intervals must be sampled"
    )
    assert name.startswith("python")

    # Already-dead root: every /proc read races an exit; the terminal case is
    # a root that never existed by sampling time. Absent, never an error.
    dead = observations._HostProcPeakSampler(proc.pid)
    dead.start()
    time.sleep(0.15)
    assert dead.stop() == (None, "")
    assert not dead._thread.is_alive()


def test_chunked_merge_keeps_one_measurement_and_never_two() -> None:
    """A chunked request is several lake invocations, so at most one of them
    can hand its measurement to the single merged payload.

    Keeping an arbitrary one of two would present a per-chunk number as if it
    described the whole request. The stale count is summed rather than
    selected, so it still describes everything the request elaborated.
    """

    def chunk(nodes, stale, cost=None):
        payload = {
            "requested_nodes": list(nodes),
            "materialized_nodes": list(nodes),
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "stale_node_count": stale,
        }
        if cost is not None:
            payload["cost"] = cost
        return payload

    cost_a = {"node": "A", "source_closure_hash": "h", "walltime_ms": 1,
              "peak_rss_kib": 2, "olean_size_bytes": 3}
    cost_b = dict(cost_a, node="B")

    one = observations._merge_materialize_chunks(
        [chunk(["A"], 1, cost_a), chunk(["B", "C"], 0)]
    )
    assert one["cost"] == cost_a
    assert one["stale_node_count"] == 1

    two = observations._merge_materialize_chunks(
        [chunk(["A"], 1, cost_a), chunk(["B"], 1, cost_b)]
    )
    assert "cost" not in two, (
        "two chunk measurements cannot both ride one payload; keeping either "
        "would look like a whole-request number"
    )
    assert two["stale_node_count"] == 2

    none = observations._merge_materialize_chunks([chunk(["A"], 0), chunk(["B"], 0)])
    assert "cost" not in none
    assert none["stale_node_count"] == 0

    # A responder that reports no count leaves the key absent rather than
    # gaining a fabricated zero: this merge presents exactly what a single
    # call would, and `test_materialize_single_chunk_keeps_response_key_set`
    # pins that key set.
    bare = {k: v for k, v in chunk(["A"], 0).items() if k != "stale_node_count"}
    assert "stale_node_count" not in observations._merge_materialize_chunks([bare])
