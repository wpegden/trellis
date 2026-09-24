"""Sidecar workspace manager (plan commit 8; amendment A8)."""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from trellis.sidecar.workspace import (
    TABLET_BUILD_REL,
    bootstrap_workspace,
    build_lake_command,
    copy_tablet_build_artifacts,
    purge_stale_oleans_for,
    refresh_to_snapshot,
    release_workspace,
    workspace_repo_path,
)


def _git(repo: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        env={
            "PATH": "/usr/bin:/bin",
            "GIT_AUTHOR_NAME": "t",
            "GIT_AUTHOR_EMAIL": "t@t",
            "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@t",
        },
    )
    assert proc.returncode == 0, proc.stderr
    return proc.stdout.strip()


NODE_FILE = (
    "import Tablet.Preamble\n\n-- [TABLET NODE: Rung]\n"
    "theorem Rung : True := by\n-- BODY\n  sorry\n"
)


@pytest.fixture()
def live_repo(tmp_path: Path) -> Path:
    repo = tmp_path / "live"
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".gitignore").write_text(".lake/\n")
    (repo / "Tablet" / "Preamble.lean").write_text("-- preamble\n")
    (repo / "Tablet" / "Rung.lean").write_text(NODE_FILE)
    # Prebuilt package olean shape for the hardlink seeder.
    pkg_build = repo / ".lake" / "packages" / "mathlib" / ".lake" / "build"
    pkg_build.mkdir(parents=True)
    (pkg_build / "Mathlib.olean").write_bytes(b"fake-olean")
    # Tablet build artifacts (to be COPIED, not hardlinked).
    tablet_build = repo / TABLET_BUILD_REL
    tablet_build.mkdir(parents=True)
    (tablet_build / "Rung.olean").write_bytes(b"rung-olean")
    (tablet_build / "Rung.olean.srcclosure").write_text("hash\n")
    _git(repo, "init", "-q")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "seed")
    return repo


def test_bootstrap_clones_and_copies_not_hardlinks(
    live_repo: Path, tmp_path: Path
) -> None:
    runtime = tmp_path / "runtime"
    result = bootstrap_workspace(live_repo, runtime)
    assert result.cloned
    repo = workspace_repo_path(runtime)
    assert (repo / "Tablet" / "Rung.lean").read_text() == NODE_FILE
    # Package olean is HARDLINKED (shared inode with the live repo).
    live_pkg = (
        live_repo / ".lake/packages/mathlib/.lake/build/Mathlib.olean"
    ).stat()
    side_pkg = (repo / ".lake/packages/mathlib/.lake/build/Mathlib.olean").stat()
    assert live_pkg.st_ino == side_pkg.st_ino, "packages must share inodes"
    # Tablet build artifacts are COPIES (two independent writers).
    live_olean = (live_repo / TABLET_BUILD_REL / "Rung.olean").stat()
    side_olean = (repo / TABLET_BUILD_REL / "Rung.olean").stat()
    assert live_olean.st_ino != side_olean.st_ino, "Tablet oleans must be copies"
    assert (repo / TABLET_BUILD_REL / "Rung.olean.srcclosure").exists()
    # Idempotent: second bootstrap neither re-clones nor re-copies.
    again = bootstrap_workspace(live_repo, runtime)
    assert not again.cloned
    assert again.tablet_artifacts_copied == 0


def test_refresh_resets_to_reachable_snapshot(live_repo: Path, tmp_path: Path) -> None:
    runtime = tmp_path / "runtime"
    bootstrap_workspace(live_repo, runtime)
    # Live repo advances.
    (live_repo / "Tablet" / "Rung.lean").write_text(
        NODE_FILE.replace("  sorry", "  trivial")
    )
    _git(live_repo, "add", "-A")
    _git(live_repo, "commit", "-qm", "close Rung")
    tip = _git(live_repo, "rev-parse", "HEAD")

    result = refresh_to_snapshot(runtime, tip)
    assert result.ok, result.reason
    assert result.head == tip
    repo = workspace_repo_path(runtime)
    assert "trivial" in (repo / "Tablet" / "Rung.lean").read_text()


def test_refresh_survives_non_fast_forward_history_rewrite(
    live_repo: Path, tmp_path: Path
) -> None:
    """A8: LastClean rewinds move master non-fast-forward; the FORCED
    tag-free fetch must follow without wedging."""
    runtime = tmp_path / "runtime"
    bootstrap_workspace(live_repo, runtime)
    base = _git(live_repo, "rev-parse", "HEAD")
    # Advance, refresh onto the advanced tip...
    (live_repo / "Tablet" / "Rung.lean").write_text(NODE_FILE + "-- wip\n")
    _git(live_repo, "add", "-A")
    _git(live_repo, "commit", "-qm", "wip")
    wip = _git(live_repo, "rev-parse", "HEAD")
    assert refresh_to_snapshot(runtime, wip).ok
    # ...then REWIND the live repo (reset --hard, abandoned line) and
    # commit a divergent replacement.
    _git(live_repo, "reset", "--hard", base)
    (live_repo / "Tablet" / "Rung.lean").write_text(NODE_FILE + "-- rewound\n")
    _git(live_repo, "add", "-A")
    _git(live_repo, "commit", "-qm", "rewound line")
    new_tip = _git(live_repo, "rev-parse", "HEAD")

    result = refresh_to_snapshot(runtime, new_tip)
    assert result.ok, f"forced fetch must follow the rewrite: {result.reason}"
    assert result.head == new_tip


def test_refresh_idles_on_unreachable_snapshot(live_repo: Path, tmp_path: Path) -> None:
    runtime = tmp_path / "runtime"
    bootstrap_workspace(live_repo, runtime)
    bogus = "0" * 40
    result = refresh_to_snapshot(runtime, bogus)
    assert not result.ok
    assert "unreachable" in result.reason
    # Empty sha / missing workspace also idle cleanly.
    assert not refresh_to_snapshot(runtime, "").ok
    assert not refresh_to_snapshot(tmp_path / "elsewhere", bogus).ok


def test_refresh_never_fetches_tags(live_repo: Path, tmp_path: Path) -> None:
    """A8: the checkpoint-tag namespace wraps after rewinds; the
    refresh must not import tags at all."""
    runtime = tmp_path / "runtime"
    bootstrap_workspace(live_repo, runtime)
    _git(live_repo, "tag", "supervisor2/checkpoint-000001")
    tip = _git(live_repo, "rev-parse", "HEAD")
    assert refresh_to_snapshot(runtime, tip).ok
    tags = _git(workspace_repo_path(runtime), "tag", "--list")
    assert "supervisor2/checkpoint-000001" not in tags.splitlines()


def test_purge_stale_oleans_uses_content_hash_gate(
    live_repo: Path, tmp_path: Path
) -> None:
    runtime = tmp_path / "runtime"
    bootstrap_workspace(live_repo, runtime)
    repo = workspace_repo_path(runtime)
    # The copied artifacts carry a bogus srcclosure sidecar => the
    # content gate reports stale => purge removes the artifacts.
    olean = repo / TABLET_BUILD_REL / "Rung.olean"
    assert olean.exists()
    # The purge pass needs the repo check-script path to exist for the
    # closure hash; absent script means fail-closed stale — fine.
    purged = purge_stale_oleans_for(repo, "Rung")
    assert "Rung" in purged
    assert not olean.exists(), "stale artifacts must be purged"


def test_bootstrap_seeds_freshness_provenance(live_repo: Path, tmp_path: Path) -> None:
    """Grunt-bench §3.1 production fix: the clone is gitignored-
    .trellis-free and a live `.lake` may carry no srcclosure sidecars,
    so without seeding the content gate fails closed on EVERY olean and
    the first attempt purges the target's whole transitive closure
    (measured 51/51, 203 s server open). Bootstrap must (1) copy
    `.trellis/scripts/check.py` from the live repo and (2) stamp exact
    `.olean.srcclosure` sidecars, after which the purge pass is a
    no-op on a fresh workspace."""
    from trellis.atomic_actions.observations import olean_is_content_current

    # Live repo shape for this scenario: check script present but
    # gitignored; oleans WITHOUT sidecars (completed runs need not
    # persist them).
    (live_repo / ".gitignore").write_text(".lake/\n.trellis/\n")
    check_py = live_repo / ".trellis" / "scripts" / "check.py"
    check_py.parent.mkdir(parents=True)
    check_py.write_text("# freshness-hashed check script\n")
    (live_repo / TABLET_BUILD_REL / "Rung.olean.srcclosure").unlink()
    (live_repo / TABLET_BUILD_REL / "Preamble.olean").write_bytes(b"preamble-olean")
    _git(live_repo, "add", "-A")
    _git(live_repo, "commit", "-qm", "freshness scenario")

    runtime = tmp_path / "runtime"
    result = bootstrap_workspace(live_repo, runtime)
    repo = workspace_repo_path(runtime)
    # (1) The clone itself never carries .trellis (gitignored)...
    tracked = _git(repo, "ls-files", ".trellis")
    assert tracked == ""
    # ...but bootstrap copied the check script across.
    assert (repo / ".trellis" / "scripts" / "check.py").read_text() == (
        "# freshness-hashed check script\n"
    )
    # (2) Sidecars stamped for the copied oleans (Rung; Preamble is not
    # a Tablet import-closure node file here — Rung is the one asserted).
    assert result.srcclosure_seeded >= 1
    assert (repo / TABLET_BUILD_REL / "Rung.olean.srcclosure").exists()
    assert olean_is_content_current(repo, "Rung")
    # The whole point: a fresh workspace purges NOTHING for the target.
    assert purge_stale_oleans_for(repo, "Rung") == []
    # Both artifacts survive the refresh's git clean (ignored paths).
    tip = _git(live_repo, "rev-parse", "HEAD")
    assert refresh_to_snapshot(runtime, tip).ok
    assert (repo / ".trellis" / "scripts" / "check.py").exists()
    assert (repo / TABLET_BUILD_REL / "Rung.olean.srcclosure").exists()
    # Existing sidecars are left untouched on a re-bootstrap.
    again = bootstrap_workspace(live_repo, runtime)
    assert again.srcclosure_seeded == 0


def test_build_lake_command_is_lowest_priority(tmp_path: Path, monkeypatch) -> None:
    # With the role explicitly disabled the inner command runs bare
    # under nice/ionice (offline harness / test path).
    import trellis.sidecar.workspace as ws

    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: False)
    cmd = ws.build_lake_command(
        tmp_path, ["lake", "build", "Tablet.Rung"], lean_threads=2, sandbox_role=""
    )
    assert cmd[:5] == ["nice", "-n", "19", "ionice", "-c3"]
    assert cmd[5:] == ["lake", "build", "Tablet.Rung"]


# ---------------------------------------------------------------------------
# F2/F5: wrapper components + fail-closed sandbox requirement
# ---------------------------------------------------------------------------


def _fake_wrap_command(cmd, sandbox=None, work_dir=None, burst_home=None, role=None):
    return ["bwrap", "--ro-bind", "/pkgs", "/pkgs", *cmd]


def test_build_lake_command_wraps_role_under_bwrap(tmp_path: Path, monkeypatch) -> None:
    import shutil as shutil_mod

    import trellis.sidecar.workspace as ws

    monkeypatch.setattr(shutil_mod, "which", lambda name: "/usr/bin/bwrap")
    monkeypatch.setattr("trellis.sandbox.wrap_command", _fake_wrap_command)
    cmd = ws.build_lake_command(
        tmp_path,
        ["lake", "build", "Tablet.Rung"],
        lean_threads=2,
        sandbox_role="lake_compiler",
    )
    assert cmd[:5] == ["nice", "-n", "19", "ionice", "-c3"]
    assert cmd[5] == "bwrap"
    assert cmd[6:9] == ["--setenv", "LEAN_NUM_THREADS", "2"]
    assert cmd[-3:] == ["lake", "build", "Tablet.Rung"]


def test_build_lake_command_refuses_without_bwrap(tmp_path: Path, monkeypatch) -> None:
    """F5 fail-closed: sandbox role configured + bwrap missing must
    REFUSE — the two-writer package-olean safety rests on the ro-bind."""
    import shutil as shutil_mod

    import trellis.sidecar.workspace as ws

    monkeypatch.setattr(shutil_mod, "which", lambda name: None)
    with pytest.raises(ws.SandboxUnavailableError, match="bwrap"):
        ws.build_lake_command(
            tmp_path,
            ["lake", "build", "Tablet.Rung"],
            lean_threads=2,
            sandbox_role="lake_compiler",
        )
    with pytest.raises(ws.SandboxUnavailableError):
        ws.build_lean_server_command(
            tmp_path, lean_threads=2, sandbox_role="lake_compiler"
        )
    with pytest.raises(ws.SandboxUnavailableError):
        ws.ensure_sandbox_available("lake_compiler", False)


def test_allow_unsandboxed_escape_hatch_is_loud(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    import shutil as shutil_mod

    import trellis.sidecar.workspace as ws

    monkeypatch.setattr(shutil_mod, "which", lambda name: None)
    cmd = ws.build_lake_command(
        tmp_path,
        ["lake", "build", "Tablet.Rung"],
        lean_threads=2,
        sandbox_role="lake_compiler",
        allow_unsandboxed=True,
    )
    assert cmd == ["nice", "-n", "19", "ionice", "-c3", "lake", "build", "Tablet.Rung"]
    assert "UNSANDBOXED" in capsys.readouterr().err


def test_build_lean_server_command_wraps_and_pins_elan(
    tmp_path: Path, monkeypatch
) -> None:
    """F2: the warm lean server gets the SAME wrapper as lake commands
    (nice/ionice/bwrap role/LEAN_NUM_THREADS) plus the prewarm recipe's
    ELAN_HOME pin and outer-PATH elan bin."""
    import shutil as shutil_mod

    import trellis.host_runtime as host_runtime
    import trellis.sidecar.workspace as ws

    monkeypatch.setattr(shutil_mod, "which", lambda name: "/usr/bin/bwrap")
    monkeypatch.setattr("trellis.sandbox.wrap_command", _fake_wrap_command)
    monkeypatch.setattr(host_runtime, "worker_elan_home", lambda: Path("/fake/elan"))
    cmd, env = ws.build_lean_server_command(
        tmp_path, lean_threads=2, sandbox_role="lake_compiler"
    )
    assert cmd[:5] == ["nice", "-n", "19", "ionice", "-c3"]
    assert cmd[5] == "bwrap"
    joined = " ".join(cmd)
    assert "--setenv LEAN_NUM_THREADS 2" in joined
    assert "--setenv ELAN_HOME /fake/elan" in joined
    assert cmd[-4:] == ["lake", "env", "lean", "--server"]
    assert env["PATH"].split(":")[0] == "/fake/elan/bin"


def test_build_lean_server_command_bare_path_caps_threads(tmp_path: Path) -> None:
    import trellis.sidecar.workspace as ws

    cmd, env = ws.build_lean_server_command(tmp_path, lean_threads=2, sandbox_role="")
    assert cmd == ["nice", "-n", "19", "ionice", "-c3", "lake", "env", "lean", "--server"]
    assert env["LEAN_NUM_THREADS"] == "2"


def test_compile_loop_launches_server_through_wrapper(
    tmp_path: Path, monkeypatch
) -> None:
    """F2 regression pin: SidecarCompileLoop must launch its warm
    server through build_lean_server_command, never bare."""
    from trellis import incremental_check as _ic
    from trellis.sidecar.compile_loop import SidecarCompileLoop
    from trellis.sidecar.config import SidecarConfig

    captured = {}

    class FakeServer:
        def __init__(self, repo, server_cmd=None, server_env=None):
            captured["cmd"] = server_cmd
            captured["env"] = server_env

        def initialize(self):
            return True

        def alive(self):
            return True

        def shutdown(self):
            pass

    monkeypatch.setattr(_ic, "_LeanServer", FakeServer)
    loop = SidecarCompileLoop(
        tmp_path, SidecarConfig(enabled=True, sandbox_role="", lean_threads=2)
    )
    server = loop._ensure_server()
    assert server is not None
    assert captured["cmd"][:5] == ["nice", "-n", "19", "ionice", "-c3"]
    assert captured["cmd"][5:] == ["lake", "env", "lean", "--server"]
    assert captured["env"]["LEAN_NUM_THREADS"] == "2"


def test_grunt_workspaces_are_isolated(live_repo: Path, tmp_path: Path) -> None:
    """Queue redesign §2.2: each grunt gets its own checkout — a bad
    write in grunt 0's tree never appears in grunt 1's."""
    runtime = tmp_path / "runtime"
    r0 = bootstrap_workspace(live_repo, runtime, grunt=0)
    r1 = bootstrap_workspace(live_repo, runtime, grunt=1)
    assert r0.cloned and r1.cloned
    repo0 = workspace_repo_path(runtime, 0)
    repo1 = workspace_repo_path(runtime, 1)
    assert repo0 != repo1
    (repo0 / "Tablet" / "Rung.lean").write_text("-- trashed by grunt 0\n")
    assert "trashed" not in (repo1 / "Tablet" / "Rung.lean").read_text()
    # refresh_to_snapshot doubles as the cancel rollback: grunt 0's
    # tree resets byte-clean while grunt 1 is untouched.
    tip = _git(live_repo, "rev-parse", "HEAD")
    assert refresh_to_snapshot(runtime, tip, grunt=0).ok
    assert "trashed" not in (repo0 / "Tablet" / "Rung.lean").read_text()


def test_released_workspace_is_reprovisioned_by_the_next_bootstrap(
    live_repo: Path, tmp_path: Path
) -> None:
    """The wind-down's reversibility claim, checked rather than assumed:
    deleting a grunt checkout is safe precisely because `bootstrap_workspace`
    re-clones it (and re-seeds packages and oleans) at the next daemon
    start, so a run revived into formalization gets a working pool back."""
    runtime = tmp_path / "runtime"
    bootstrap_workspace(live_repo, runtime, grunt=1)
    repo = workspace_repo_path(runtime, grunt=1)
    log = repo.parent / "attempt-sc-1.log"
    log.write_text("forensics")

    assert release_workspace(runtime, 1)
    assert not repo.exists()
    assert log.exists(), "only the checkout is released"
    # Releasing what is already gone is not an error (the wind-down is
    # idempotent across restarts).
    assert not release_workspace(runtime, 1)

    again = bootstrap_workspace(live_repo, runtime, grunt=1)
    assert again.cloned
    assert (repo / "Tablet" / "Rung.lean").read_text() == NODE_FILE
    assert (repo / ".lake/packages/mathlib/.lake/build/Mathlib.olean").exists()
    assert (repo / TABLET_BUILD_REL / "Rung.olean").exists()
