from __future__ import annotations

from pathlib import Path
import os
import shutil
import subprocess

import pytest

from trellis.config import SandboxConfig
from trellis.host_runtime import worker_path_env
from trellis.sandbox import (
    _trellis_source_scripts_dir,
    _repo_write_violations,
    certify_worker_checker_surface,
    wrap_command,
)
from trellis.worker_scratch import worker_scratch_dir, worker_scratch_notes_path


def _contains_bind(cmd: list[str], mode: str, path: Path) -> bool:
    needle = [mode, str(path), str(path)]
    for idx in range(len(cmd) - 2):
        if cmd[idx : idx + 3] == needle:
            return True
    return False


def _cmd_has_flag(cmd: list[str], flag: str) -> bool:
    """Return True iff ``flag`` appears verbatim in the bwrap argv.

    Phase 1 (SANDBOX_BWRAP_ONLY_MIGRATION_PLAN_2026-06-03.md §3) asserts
    that the per-role bwrap line is hardened with `--unshare-pid`,
    `--unshare-ipc`, `--unshare-uts`, and `--cap-drop ALL`. Helper kept
    name-only (no positional assumption) so future arg-order changes
    don't break the per-role assertions.
    """
    return flag in cmd


def _assert_phase1_hardening(cmd: list[str]) -> None:
    """Assert the Phase 1 bwrap hardening flags are present on the argv."""
    assert _cmd_has_flag(cmd, "--unshare-pid"), cmd
    assert _cmd_has_flag(cmd, "--unshare-ipc"), cmd
    assert _cmd_has_flag(cmd, "--unshare-uts"), cmd
    assert _cmd_has_flag(cmd, "--cap-drop"), cmd
    # `--cap-drop` is followed by its argument; ensure the value is ALL.
    idx = cmd.index("--cap-drop")
    assert cmd[idx + 1] == "ALL", cmd


def test_pv_source_commands_share_configured_lean_libraries(tmp_path: Path, monkeypatch) -> None:
    import json
    import os
    import subprocess

    from trellis.modeling.environment import source_environment

    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.setenv("LEAN_PATH", str(tmp_path / "stale-library"))
    repo = tmp_path / "repo"
    (repo / "modeling-input").mkdir(parents=True)
    paths = [str(tmp_path / "library one"), str(tmp_path / "library-two")]
    tools = {"lean_paths": paths}
    (repo / "modeling-input/manifest.json").write_text(json.dumps({"effective_tools": tools}))
    expected = os.pathsep.join(paths)
    assert source_environment(tools, repo / ".trellis/scratch/direct")["LEAN_PATH"] == expected

    for role in ("worker", "reviewer"):
        cmd = wrap_command(
            ["true"], sandbox=SandboxConfig(enabled=True, backend="bwrap"),
            work_dir=repo, burst_home=tmp_path / role, role=role,
        )
        env = {cmd[i + 1]: cmd[i + 2] for i, flag in enumerate(cmd) if flag == "--setenv"}
        assert env["LEAN_PATH"] == expected
        shell = subprocess.run(
            ["/bin/bash", "-lc", 'printf %s "$LEAN_PATH"'],
            env={**os.environ, **env}, capture_output=True, text=True, check=True,
        )
        assert shell.stdout == expected


@pytest.mark.parametrize("hoisted", [False, True])
def test_reviewer_can_execute_npm_dependency_inside_bwrap(tmp_path: Path, monkeypatch, hoisted: bool) -> None:
    """Exercise Node resolution in a real sandbox, not just argv spelling."""
    if not shutil.which("bwrap") or not shutil.which("node"):
        pytest.skip("requires bubblewrap and Node")
    node_binary = Path(shutil.which("node")).resolve()
    probe = subprocess.run(
        ["bwrap", "--ro-bind", "/", "/", "--unshare-pid", "/bin/true"],
        capture_output=True, text=True, timeout=10,
    )
    if probe.returncode:
        pytest.skip(f"bubblewrap namespaces unavailable: {probe.stderr.strip()}")

    operator = tmp_path / "operator"
    prefix = operator / ".local/share/npm-global"
    modules = prefix / "lib/node_modules"
    package = modules / "@openai/codex"
    bin_dir = package / "bin"
    bin_dir.mkdir(parents=True)
    dependency = (modules if hoisted else package / "node_modules") / "@openai/codex-linux-x64"
    dependency.mkdir(parents=True)
    (dependency / "package.json").write_text('{"name":"@openai/codex-linux-x64"}')
    (dependency / "native").write_text("#!/bin/sh\necho native-ok\n")
    (dependency / "native").chmod(0o755)
    target = bin_dir / "codex.js"
    target.write_text(
        "#!/usr/bin/env node\n"
        "const fs = require('fs'), path = require('path');\n"
        "const root = path.dirname(require.resolve('@openai/codex-linux-x64/package.json'));\n"
        "if (fs.existsSync(process.argv[2])) throw Error('operator secret exposed');\n"
        "try { fs.writeFileSync(path.join(root, 'write-probe'), 'bad'); process.exit(99); }\n"
        "catch (e) { if (e.code !== 'EROFS') throw e; }\n"
        "require('child_process').execFileSync(path.join(root, 'native'), {stdio: 'inherit'});\n"
    )
    target.chmod(0o755)
    launcher = prefix / "bin/codex"
    launcher.parent.mkdir()
    launcher.symlink_to("../lib/node_modules/@openai/codex/bin/codex.js")
    # Supply Node in the fixture's launcher PATH even on hosts using nvm.
    # The test exercises npm dependency resolution, independently of host PATH.
    (launcher.parent / "node").symlink_to(node_binary)
    secret = operator / "private.txt"
    secret.write_text("must remain outside the sandbox")
    monkeypatch.setattr(Path, "home", lambda: operator)
    monkeypatch.setattr("trellis.host_runtime.shutil.which", lambda name: str(launcher) if name == "codex" else None)
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.setattr("trellis.sandbox._host_extra_readonly_paths", lambda: [node_binary.parent])
    monkeypatch.setattr("trellis.sandbox._env_extra_readonly_paths", lambda environment=None: [])
    monkeypatch.setattr("trellis.sandbox._reviewer_source_snapshot", lambda: None)
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    burst_home = tmp_path / "burst-home"
    burst_home.mkdir()
    cmd = wrap_command(
        [str(launcher), str(secret)], sandbox=SandboxConfig(),
        work_dir=repo, burst_home=burst_home, role="reviewer",
    )
    assert _contains_bind(cmd, "--ro-bind", modules)
    assert not _contains_bind(cmd, "--bind", modules)
    result = subprocess.run(
        cmd, env={**os.environ, "PATH": worker_path_env(burst_home)},
        capture_output=True, text=True, timeout=20,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "native-ok"
    compiler = wrap_command(
        ["true"], sandbox=SandboxConfig(), work_dir=repo,
        burst_home=burst_home, role="lake_compiler",
    )
    assert not _contains_bind(compiler, "--ro-bind", modules)


def test_worker_sandbox_keeps_repo_read_only_except_explicit_writes(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    # Pre-flag-flip ("legacy direct-lake") path: worker burst still runs lake
    # in-burst, so the build outputs and cheat-trace dir must remain in the
    # writable allowlist. The Step-5 RPC tightening only fires when
    # TRELLIS_CHECKER_SOCKET is set; clear it here so this test pins legacy
    # behavior even if the surrounding shell exports the var.
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".trellis" / "runtime" / "rt1").mkdir(parents=True)
    (repo / ".lake" / "packages" / "mathlib").mkdir(parents=True)
    (repo / ".lake" / "packages" / "proofwidgets" / "widget").mkdir(parents=True)

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )

    _assert_phase1_hardening(cmd)
    assert ["--symlink", "/trellis-tmp", "/tmp"] == cmd[cmd.index("--symlink"):cmd.index("--symlink") + 3]
    tmp_bind = next(cmd[idx + 1] for idx in range(len(cmd) - 2)
                    if cmd[idx] == "--bind" and cmd[idx + 2] == "/trellis-tmp")
    assert Path(tmp_bind).is_relative_to(repo / ".trellis/tmp")
    assert not _contains_bind(cmd, "--bind", Path("/tmp"))
    assert not _contains_bind(cmd, "--ro-bind", Path("/tmp"))
    assert _contains_bind(cmd, "--ro-bind", repo)
    assert _contains_bind(cmd, "--bind", repo / "Tablet")
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "scratch")
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "tmp")
    assert _contains_bind(cmd, "--bind", repo / ".lake" / "build")
    assert _contains_bind(cmd, "--bind", repo / ".lake" / "packages" / "mathlib" / ".lake" / "build")
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "staging")
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "runtime" / "rt1" / "staging")
    assert _contains_bind(cmd, "--bind", repo / ".lake" / "packages" / "proofwidgets" / "widget")
    # checker/ must be writable — worker-side trace lands there; without it the
    # bridge's cross-check between worker and supervisor validation silently
    # disables because the worker's record_worker_checker_trace hits EROFS.
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "checker")
    assert (repo / ".trellis" / "checker").is_dir(), (
        "wrap_command should have pre-created .trellis/checker on host"
    )
    assert not _contains_bind(cmd, "--bind", repo / ".lake" / "packages")
    assert not _contains_bind(cmd, "--bind", repo / ".git")
    assert not _contains_bind(cmd, "--bind", repo / "Tablet.lean")
    assert not _contains_bind(cmd, "--bind", repo / "work")

    sandbox = SandboxConfig(extra_writable_paths=("work",))
    extended = wrap_command(
        ["echo", "ok"], sandbox=sandbox, work_dir=repo,
        burst_home=Path("/home/sandbox-user"), role="worker",
    )
    assert _contains_bind(extended, "--bind", repo / "work")
    assert _repo_write_violations(
        repo, role="worker", changed_paths=[repo / "work/crate/lib.rs"], sandbox=sandbox,
    ) == []
    assert _repo_write_violations(
        repo, role="worker", changed_paths=[repo / "work/crate/lib.rs"],
    ) == [repo / "work/crate/lib.rs"]

    import pytest
    (repo / "outside").symlink_to(tmp_path)
    for path in (".", "..", str(tmp_path), "outside/file"):
        with pytest.raises(ValueError, match="inside the repository"):
            wrap_command(
                ["echo", "ok"], sandbox=SandboxConfig(extra_writable_paths=(path,)),
                work_dir=repo, burst_home=Path("/home/sandbox-user"), role="worker",
            )


def _write_target_config(repo: Path, target: str | None) -> None:
    import json

    cfg = {"workflow": {"default_target": target}} if target else {}
    (repo / "trellis.config.json").write_text(json.dumps(cfg), encoding="utf-8")


def test_isabelle_run_binds_distribution_and_heap_readonly(
    tmp_path: Path, monkeypatch
) -> None:
    """GAP 1: on an isabelle_hol run the worker/reviewer/verifier bwrap gains
    RO binds for the Isabelle distribution + the warm heap, the
    ISABELLE_HOME_USER / TRELLIS_ISABELLE_BIN setenvs, and (critically) the
    heap is bound read-only so an in-burst `isabelle build` cannot clobber it.
    """
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)

    isa_home = tmp_path / "Isabelle2025-2"
    (isa_home / "bin").mkdir(parents=True)
    isa_user = tmp_path / "dot-isabelle" / "Isabelle2025-2"
    (isa_user / "heaps").mkdir(parents=True)
    monkeypatch.setattr("trellis.sandbox.worker_isabelle_home", lambda: isa_home)
    monkeypatch.setattr("trellis.sandbox.worker_isabelle_home_user", lambda: isa_user)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    _write_target_config(repo, "isabelle_hol")

    for role in ("worker", "reviewer", "verifier"):
        cmd = wrap_command(
            ["echo", "ok"],
            sandbox=SandboxConfig(enabled=True, backend="bwrap"),
            work_dir=repo,
            burst_home=Path("/home/sandbox-user"),
            role=role,
        )
        # Distribution + heap are RO-bound (heap is NEVER --bind/writable).
        assert _contains_bind(cmd, "--ro-bind", isa_home), role
        assert _contains_bind(cmd, "--ro-bind", isa_user), role
        assert not _contains_bind(cmd, "--bind", isa_user), role
        # Env wiring so the in-burst isabelle finds the heap + absolute launcher.
        assert "ISABELLE_HOME_USER" in cmd, role
        assert str(isa_user) in cmd, role
        assert "TRELLIS_ISABELLE_BIN" in cmd, role
        assert str(isa_home / "bin" / "isabelle") in cmd, role
        # Poly/ML spools to a hard-coded `/tmp` (ignores $TMPDIR), so isabelle
        # runs alias `/tmp` to the repository-backed temporary directory.
        assert any(cmd[i:i + 3] == ["--symlink", "/trellis-tmp", "/tmp"]
                   for i in range(len(cmd) - 2)), role
        # `.lake/build` is the LEAN worker's local olean scratch; an Isabelle
        # worker has no lake step, so it is neither bound nor created on host —
        # no Lean residue on an isabelle repo.
        assert not _contains_bind(cmd, "--bind", repo / ".lake" / "build"), role
        assert not (repo / ".lake").exists(), role

    # lake_compiler is supervisor-side + Lean-only; it never gets the bind.
    lc_cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="lake_compiler",
    )
    assert not _contains_bind(lc_cmd, "--ro-bind", isa_home)
    assert "TRELLIS_ISABELLE_BIN" not in lc_cmd


def test_isabelle_heap_inside_home_is_refused(tmp_path: Path, monkeypatch) -> None:
    """Defense-in-depth: if the Isabelle heap dir is INSIDE the sandbox HOME, the
    RO heap bind would be shadowed by the writable HOME bind — giving an in-burst
    `isabelle build` a writable heap. wrap_command must refuse loudly, and the
    normal (heap outside HOME) path must still bind RO without raising.
    """
    import pytest

    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)

    isa_home = tmp_path / "Isabelle2025-2"
    (isa_home / "bin").mkdir(parents=True)
    monkeypatch.setattr("trellis.sandbox.worker_isabelle_home", lambda: isa_home)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    _write_target_config(repo, "isabelle_hol")

    burst_home = tmp_path / "burst-home"
    # Heap dir placed INSIDE the burst home → the RO bind is unsafe.
    heap_inside = burst_home / ".isabelle" / "Isabelle2025-2"
    (heap_inside / "heaps").mkdir(parents=True)
    monkeypatch.setattr("trellis.sandbox.worker_isabelle_home_user",
                        lambda: heap_inside)
    with pytest.raises(RuntimeError, match="inside the sandbox HOME"):
        wrap_command(
            ["echo", "ok"],
            sandbox=SandboxConfig(enabled=True, backend="bwrap"),
            work_dir=repo,
            burst_home=burst_home,
            role="worker",
        )

    # Heap dir OUTSIDE the burst home → bound RO, no raise (the production shape).
    heap_outside = tmp_path / "dot-isabelle" / "Isabelle2025-2"
    (heap_outside / "heaps").mkdir(parents=True)
    monkeypatch.setattr("trellis.sandbox.worker_isabelle_home_user",
                        lambda: heap_outside)
    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=burst_home,
        role="worker",
    )
    assert _contains_bind(cmd, "--ro-bind", heap_outside)


def test_lean_run_has_no_isabelle_binds(tmp_path: Path, monkeypatch) -> None:
    """Behavior-preservation: a Lean (or target-unset) repo never gains an
    Isabelle bind/setenv on any role, so the Lean bwrap argv is unchanged."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)
    isa_home = tmp_path / "Isabelle2025-2"
    (isa_home / "bin").mkdir(parents=True)
    monkeypatch.setattr("trellis.sandbox.worker_isabelle_home", lambda: isa_home)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    for target in (None, "lean"):
        _write_target_config(repo, target)
        for role in ("worker", "reviewer", "verifier"):
            cmd = wrap_command(
                ["echo", "ok"],
                sandbox=SandboxConfig(enabled=True, backend="bwrap"),
                work_dir=repo,
                burst_home=Path("/home/sandbox-user"),
                role=role,
            )
            assert not _contains_bind(cmd, "--ro-bind", isa_home), (target, role)
            assert "TRELLIS_ISABELLE_BIN" not in cmd, (target, role)
            assert "ISABELLE_HOME_USER" not in cmd, (target, role)
            # The isabelle-only `/tmp` tmpfs must not leak into the Lean argv.
            tmpfs_targets = [cmd[i + 1] for i, a in enumerate(cmd)
                             if a == "--tmpfs"]
            assert "/tmp" not in tmpfs_targets, (target, role)


def test_worker_sandbox_keeps_lake_and_checker_paths_with_checker_socket_set(
    tmp_path: Path, monkeypatch
) -> None:
    """The worker keeps `.lake/build`, per-package `.lake/build`, per-package
    `widget/`, and `.trellis/checker` writable EVEN when TRELLIS_CHECKER_SOCKET
    is set. Workers still need a fast inner edit-compile-fix loop
    (`lake build Tablet.NodeName`, `lake env lean Tablet/X.lean`, scratch
    builds) — those all need to write oleans. Sign-off authority lives on
    the supervisor side (.trellis/supervisor/repo/.lake/build), so any
    drift in the worker's local oleans is caught at the deterministic-check
    gate.
    """
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.setenv("TRELLIS_CHECKER_SOCKET", "/tmp/trellis-checker.sock")

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".trellis" / "runtime" / "rt1").mkdir(parents=True)
    (repo / ".lake" / "packages" / "mathlib").mkdir(parents=True)
    (repo / ".lake" / "packages" / "proofwidgets" / "widget").mkdir(parents=True)

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )

    # Repo still ro-bind, Tablet still writable.
    assert _contains_bind(cmd, "--ro-bind", repo)
    assert _contains_bind(cmd, "--bind", repo / "Tablet")
    # Generic state-dir scratch dirs remain writable.
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "scratch")
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "tmp")
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "staging")
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "runtime" / "rt1" / "staging")
    # Build/cheat paths MUST appear writable so lake build / lake env lean works.
    assert _contains_bind(cmd, "--bind", repo / ".lake" / "build")
    assert _contains_bind(
        cmd, "--bind", repo / ".lake" / "packages" / "mathlib" / ".lake" / "build"
    )
    assert _contains_bind(
        cmd, "--bind", repo / ".lake" / "packages" / "proofwidgets" / "widget"
    )
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "checker")


def test_worker_sandbox_forwards_checker_token_env_when_set(
    tmp_path: Path, monkeypatch
) -> None:
    """Phase 2 (bwrap-only migration plan): ``TRELLIS_CHECKER_TOKEN`` is
    in ``_PASSTHROUGH_VALUE_ENV_VARS`` so the bridge-minted token reaches
    the worker burst via ``--setenv``. The runtime root is not bind-mounted
    into the burst, so this env-var path is the ONLY channel by which
    the burst learns its own token."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.setenv("TRELLIS_CHECKER_TOKEN", "burst-token-abcdef")

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )
    setenv_pairs = []
    for idx in range(len(cmd) - 2):
        if cmd[idx] == "--setenv":
            setenv_pairs.append((cmd[idx + 1], cmd[idx + 2]))
    assert ("TRELLIS_CHECKER_TOKEN", "burst-token-abcdef") in setenv_pairs


def test_worker_sandbox_omits_checker_token_setenv_when_unset(
    tmp_path: Path, monkeypatch
) -> None:
    """Without ``TRELLIS_CHECKER_TOKEN`` in the supervisor env, no token
    setenv is emitted — the burst's checker_client then sends requests
    without an ``auth_token`` field, which the server admits under the
    dormant-Phase-2 fallback path."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.delenv("TRELLIS_CHECKER_TOKEN", raising=False)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )
    setenv_names = []
    for idx in range(len(cmd) - 2):
        if cmd[idx] == "--setenv":
            setenv_names.append(cmd[idx + 1])
    assert "TRELLIS_CHECKER_TOKEN" not in setenv_names


def test_agent_sandboxes_bind_checker_socket_dir_when_env_var_set(
    tmp_path: Path, monkeypatch
) -> None:
    """Authoring roles reach the checker; reviewer and audit keep their read-only scope."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    runtime_root = tmp_path / "example-run-runtime"
    socket_dir = runtime_root / "sockets"
    socket_dir.mkdir(parents=True)
    socket_path = socket_dir / "checker.sock"
    monkeypatch.setenv("TRELLIS_CHECKER_SOCKET", str(socket_path))

    for role in ("worker", "reviewer", "stuck_math_audit"):
        cmd = wrap_command(
            ["echo", "ok"], sandbox=SandboxConfig(enabled=True, backend="bwrap"),
            work_dir=repo, burst_home=Path("/home/sandbox-user"), role=role,
        )
        assert _contains_bind(cmd, "--ro-bind", socket_dir.resolve()) == (role == "worker")
        assert not _contains_bind(cmd, "--bind", socket_dir.resolve())
        setenv_pairs = [(cmd[idx + 1], cmd[idx + 2]) for idx in range(len(cmd) - 2)
                        if cmd[idx] == "--setenv"]
        assert ("TRELLIS_CHECKER_SOCKET", str(socket_path)) in setenv_pairs


def test_worker_sandbox_omits_checker_socket_bind_when_env_var_unset(
    tmp_path: Path, monkeypatch
) -> None:
    """Non-RPC path: when TRELLIS_CHECKER_SOCKET is unset, no socket
    bind appears and behaviour is unchanged from pre-fix. Pins the gate
    so a future refactor can't accidentally bind a stray host directory
    when the operator hasn't opted into RPC mode."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    bogus_socket_dir = tmp_path / "example-run-runtime" / "sockets"
    bogus_socket_dir.mkdir(parents=True)

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )

    assert not _contains_bind(cmd, "--ro-bind", bogus_socket_dir.resolve())
    setenv_names = []
    for idx in range(len(cmd) - 2):
        if cmd[idx] == "--setenv":
            setenv_names.append(cmd[idx + 1])
    assert "TRELLIS_CHECKER_SOCKET" not in setenv_names


def test_lake_compiler_sandbox_omits_checker_socket_bind_even_when_env_var_set(
    tmp_path: Path, monkeypatch
) -> None:
    """The lake_compiler role services the unified-checker RPC directly
    (see ``observations.py`` ``bwrap_role`` recursion guard) — it does
    not re-enter the socket client. Adding a socket bind would
    needlessly widen the supervisor-side blast radius. Pin the
    role gate so a future refactor can't accidentally extend the worker's
    socket-bind to the supervisor side."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    runtime_root = tmp_path / "example-run-runtime"
    socket_dir = runtime_root / "sockets"
    socket_dir.mkdir(parents=True)
    socket_path = socket_dir / "checker.sock"
    monkeypatch.setenv("TRELLIS_CHECKER_SOCKET", str(socket_path))

    cmd = wrap_command(
        ["lake", "build", "Tablet"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=tmp_path / "home",
        role="lake_compiler",
    )

    assert not _contains_bind(cmd, "--ro-bind", socket_dir.resolve())


def test_lake_compiler_sandbox_keeps_tablet_source_readonly_in_rpc_mode(
    tmp_path: Path, monkeypatch
) -> None:
    """Step 5: the lake_compiler role is the supervisor-side bwrap that
    runs lake on behalf of the unified-checker RPC server. Build and probe
    scratch paths stay writable, but Tablet source must remain under the
    repo-wide read-only bind so elaborator code cannot rewrite what the
    supervisor is certifying.
    """
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.setenv("TRELLIS_CHECKER_SOCKET", "/tmp/trellis-checker.sock")

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".lake" / "packages" / "mathlib").mkdir(parents=True)

    cmd = wrap_command(
        ["lake", "build", "Tablet"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=tmp_path / "home",
        role="lake_compiler",
    )

    assert _contains_bind(cmd, "--bind", repo / ".lake" / "build")
    assert _contains_bind(
        cmd, "--bind", repo / ".lake" / "packages" / "mathlib" / ".lake" / "build"
    )
    assert _contains_bind(cmd, "--ro-bind", repo)
    assert not _contains_bind(cmd, "--bind", repo / "Tablet")
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "tmp")
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "staging")


def test_lake_compiler_sandbox_keeps_tablet_source_readonly_without_socket(
    tmp_path: Path, monkeypatch
) -> None:
    """The source read-only invariant is independent of RPC env wiring.
    """
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".lake" / "packages" / "mathlib").mkdir(parents=True)

    cmd = wrap_command(
        ["lake", "build", "Tablet"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=tmp_path / "home",
        role="lake_compiler",
    )

    assert _contains_bind(cmd, "--bind", repo / ".lake" / "build")
    assert _contains_bind(
        cmd, "--bind", repo / ".lake" / "packages" / "mathlib" / ".lake" / "build"
    )
    assert _contains_bind(cmd, "--ro-bind", repo)
    assert not _contains_bind(cmd, "--bind", repo / "Tablet")


def test_lake_compiler_overlays_olean_provenance_readonly_after_build_bind(
    tmp_path: Path, monkeypatch
) -> None:
    """A writable olean directory must not make replay authority forgeable."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    provenance = (
        repo
        / ".lake"
        / "build"
        / "lib"
        / "lean"
        / "Tablet"
        / "A.olean.srcclosure"
    )
    provenance.parent.mkdir(parents=True)
    provenance.touch()

    cmd = wrap_command(
        ["lake", "build", "Tablet.A"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=tmp_path / "home",
        role="lake_compiler",
    )
    writable = [
        "--bind",
        str(repo / ".lake" / "build"),
        str(repo / ".lake" / "build"),
    ]
    protected = ["--ro-bind", str(provenance), str(provenance)]
    writable_at = next(
        i for i in range(len(cmd) - 2) if cmd[i : i + 3] == writable
    )
    protected_at = next(
        i for i in range(len(cmd) - 2) if cmd[i : i + 3] == protected
    )
    assert protected_at > writable_at, "the per-file RO overlay must win"


def test_lake_compiler_package_overlay_preserves_selected_library_mounts(
    tmp_path: Path, monkeypatch,
) -> None:
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    repo = tmp_path / "repo"
    packages = repo / ".lake/packages"
    library = packages / "mathlib/.lake/build/lib/lean"
    library.mkdir(parents=True)
    selected = tmp_path / "selected/mathlib/.lake/build/lib/lean"
    selected.mkdir(parents=True)
    cmd = wrap_command(
        ["true"], sandbox=SandboxConfig(), work_dir=repo,
        burst_home=tmp_path / "home", role="lake_compiler",
        environment={"TRELLIS_PINNED_LEAN_LIBRARIES": str(selected)},
    )
    overlay = _bind_index(cmd, "--ro-bind", packages)
    assert overlay > _bind_index(cmd, "--bind", packages / "mathlib/.lake/build")
    selected_bind = ["--ro-bind", str(selected), str(library)]
    assert any(cmd[i:i + 3] == selected_bind for i in range(overlay + 1, len(cmd) - 2))


def test_lake_compiler_cannot_write_shared_package_aliases(tmp_path: Path, monkeypatch) -> None:
    """Use the real checker UID/mount graph, including a nested dependency."""
    import json
    from trellis.supervisor_workspace import _ensure_supervisor_lake_packages

    if not shutil.which("bwrap"):
        pytest.skip("requires bubblewrap")
    probe = subprocess.run(
        ["bwrap", "--ro-bind", "/", "/", "--unshare-pid", "/bin/true"],
        capture_output=True, text=True, timeout=10,
    )
    if probe.returncode:
        pytest.skip(f"bubblewrap namespaces unavailable: {probe.stderr.strip()}")
    worker = tmp_path / "worker"
    repo = worker / ".trellis/supervisor/repo"
    home = repo.parent / "home"
    home.mkdir(parents=True)
    shared = worker / ".lake/packages/dep/backends/lean/.lake/build/lib/lean/Dep.olean"
    shared.parent.mkdir(parents=True)
    shared.write_bytes(b"host-prewarmed")
    shared.chmod(0o600)
    package = worker / ".lake/packages/dep"
    (package / "alias").symlink_to("backends/lean/.lake/build/lib/lean", target_is_directory=True)
    _ensure_supervisor_lake_packages(worker, repo)
    local = repo / shared.relative_to(worker)
    alias = repo / ".lake/packages/dep/alias/Dep.olean"
    assert local.samefile(shared)
    # Also expose the original package at its real absolute path, as a pinned
    # external library can be. It must remain read-only through that alias.
    monkeypatch.setenv("TRELLIS_SANDBOX_EXTRA_RO_PATHS", str(package))
    script = """
import errno, json, os, pathlib, sys
paths = list(map(pathlib.Path, sys.argv[1:4]))
assert os.geteuid() == paths[0].stat().st_uid
for path in paths:
    for operation in (lambda: path.write_bytes(b'forged'),
                      lambda: path.chmod(0o666),
                      lambda: path.unlink(),
                      lambda: (path.parent / 'Injected.olean').write_bytes(b'forged')):
        try:
            operation()
        except OSError as error:
            assert error.errno in (errno.EROFS, errno.EACCES, errno.EPERM), str(error)
        else:
            raise AssertionError('writable package alias: ' + str(path))
project = pathlib.Path(sys.argv[4])
project.write_bytes(b'project output')
print(json.dumps({'uid': os.geteuid(), 'package_write_denied': len(paths), 'project_write': True}))
"""
    project = repo / ".lake/build/probe-output"
    cmd = wrap_command(
        ["/usr/bin/python3", "-c", script, str(local), str(alias), str(shared), str(project)],
        sandbox=SandboxConfig(), work_dir=repo, burst_home=home, role="lake_compiler",
    )
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=20)
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout) == {
        "uid": os.geteuid(), "package_write_denied": 3, "project_write": True,
    }
    assert shared.read_bytes() == b"host-prewarmed"
    assert project.read_bytes() == b"project output"


def test_runtime_private_dir_is_read_only_for_every_burst_role(
    tmp_path: Path, monkeypatch
) -> None:
    """Audit followup: SIGHUP-recovery loads `<canonical>.acceptance.json`
    from `repo/.trellis/runtime/<name>/private/` as the trusted
    normalization baseline. If the worker (or any other burst role) can
    write there, a dishonest worker can overwrite the baseline between
    writing `.done` and a supervisor restart, and the recovered
    normalization absorbs unauthorized writes into the baseline. This
    test pins the invariant: private/ never joins the writable
    allowlist for any role."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    private_dir = repo / ".trellis" / "runtime" / "rt1" / "private"
    private_dir.mkdir(parents=True)

    for role in ("worker", "reviewer", "paper", "corr", "sound", "stuck_math_audit"):
        cmd = wrap_command(
            ["echo", "ok"],
            sandbox=SandboxConfig(enabled=True, backend="bwrap"),
            work_dir=repo,
            burst_home=Path("/home/sandbox-user"),
            role=role,
        )
        # Phase 1 bwrap hardening must hold for every role, not just the
        # named lake/worker/reviewer paths exercised earlier in this file.
        _assert_phase1_hardening(cmd)
        assert _contains_bind(cmd, "--ro-bind", repo), (
            f"role={role}: repo must be ro-bind mounted (gives the worker "
            f"read access to .acceptance.json without a per-file binding)"
        )
        assert not _contains_bind(cmd, "--bind", private_dir), (
            f"role={role}: private/ must NOT be in the writable allowlist "
            f"(would let the role overwrite the recovery baseline)"
        )


def test_reviewer_sandbox_cannot_write_tablet_sources(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="reviewer",
    )

    _assert_phase1_hardening(cmd)
    assert _contains_bind(cmd, "--ro-bind", repo)
    assert not _contains_bind(cmd, "--bind", repo / "Tablet")
    assert not _contains_bind(cmd, "--bind", repo / "Tablet.lean")


def test_reviewer_sandbox_mounts_source_recourse_snapshot_when_env_vars_set(
    tmp_path: Path, monkeypatch
) -> None:
    """When TRELLIS_REVIEWER_SOURCE_SNAPSHOT is set, the reviewer's bwrap
    mounts that path read-only so the reviewer can consult kernel + Python
    source as a fallback when process semantics seem to block forward
    progress. The snapshot is materialized once per run by
    `scripts/trellis.sh` at a specific git SHA."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    snapshot_dir = tmp_path / "trellis-source-snapshot" / "deadbeef"
    snapshot_dir.mkdir(parents=True)

    monkeypatch.setenv("TRELLIS_REVIEWER_SOURCE_SNAPSHOT", str(snapshot_dir))
    monkeypatch.setenv("TRELLIS_REVIEWER_SOURCE_SHA", "deadbeef")

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="reviewer",
    )

    assert _contains_bind(cmd, "--ro-bind", snapshot_dir)
    # The setenv instructions must also forward both env vars so the
    # reviewer's prompt placeholders can be filled inside the sandbox.
    setenv_pairs = []
    for idx in range(len(cmd) - 2):
        if cmd[idx] == "--setenv":
            setenv_pairs.append((cmd[idx + 1], cmd[idx + 2]))
    assert ("TRELLIS_REVIEWER_SOURCE_SNAPSHOT", str(snapshot_dir)) in setenv_pairs
    assert ("TRELLIS_REVIEWER_SOURCE_SHA", "deadbeef") in setenv_pairs


def test_reviewer_sandbox_omits_source_recourse_when_env_vars_unset(
    tmp_path: Path, monkeypatch
) -> None:
    """Silent-degradation path: when the env vars are unset, the reviewer
    runs with no source recourse mount and no setenv leakage. The kernel
    omits the corresponding prompt fragment in the same condition, so
    nothing references the missing snapshot path."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.delenv("TRELLIS_REVIEWER_SOURCE_SNAPSHOT", raising=False)
    monkeypatch.delenv("TRELLIS_REVIEWER_SOURCE_SHA", raising=False)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    bogus_dir = tmp_path / "would-not-exist"

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="reviewer",
    )

    assert not _contains_bind(cmd, "--ro-bind", bogus_dir)
    setenv_names = []
    for idx in range(len(cmd) - 2):
        if cmd[idx] == "--setenv":
            setenv_names.append(cmd[idx + 1])
    assert "TRELLIS_REVIEWER_SOURCE_SNAPSHOT" not in setenv_names
    assert "TRELLIS_REVIEWER_SOURCE_SHA" not in setenv_names


def test_worker_sandbox_does_not_mount_source_recourse_snapshot(
    tmp_path: Path, monkeypatch
) -> None:
    """The source-recourse snapshot is reviewer-only by design. Workers
    must not get a parallel read-only kernel-source mount — the
    cheat-detection model assumes the worker only sees the project
    repository as ro-bind, plus its declared writable allowlist."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    snapshot_dir = tmp_path / "trellis-source-snapshot" / "deadbeef"
    snapshot_dir.mkdir(parents=True)

    monkeypatch.setenv("TRELLIS_REVIEWER_SOURCE_SNAPSHOT", str(snapshot_dir))
    monkeypatch.setenv("TRELLIS_REVIEWER_SOURCE_SHA", "deadbeef")

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )

    assert not _contains_bind(cmd, "--ro-bind", snapshot_dir)


def _ro_bind_covers(cmd: list[str], path: Path) -> bool:
    """Is ``path`` readable inside the sandbox via some ``--ro-bind``?

    Unlike ``_contains_bind`` this accepts an ANCESTOR bind: a directory
    bound at its own path makes every file under it resolve too.
    """
    for idx in range(len(cmd) - 2):
        if cmd[idx] != "--ro-bind":
            continue
        src, dst = Path(cmd[idx + 1]), Path(cmd[idx + 2])
        if src != dst:
            continue
        if path == dst or path.is_relative_to(dst):
            return True
    return False


def _writable_bind_covers(cmd: list[str], path: Path) -> bool:
    """Same as ``_ro_bind_covers`` for read-WRITE ``--bind`` mounts."""
    for idx in range(len(cmd) - 2):
        if cmd[idx] != "--bind":
            continue
        src, dst = Path(cmd[idx + 1]), Path(cmd[idx + 2])
        if src != dst:
            continue
        if path == dst or path.is_relative_to(dst):
            return True
    return False


def test_worker_write_certification_rejects_tablet_root_and_package_source_writes(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".lake" / "packages" / "mathlib" / ".lake" / "build").mkdir(parents=True)
    changed_paths = [
        repo / "Tablet.lean",
        repo / ".lake" / "packages" / "mathlib" / ".git" / "index",
    ]

    violations = _repo_write_violations(repo, role="worker", changed_paths=changed_paths)

    assert repo.joinpath("Tablet.lean").resolve() in violations
    assert repo.joinpath(".lake/packages/mathlib/.git/index").resolve() in violations


def test_worker_write_certification_allows_declared_worker_write_surface(
    tmp_path: Path,
    monkeypatch,
) -> None:
    # Worker writes to `.lake/build/...A.olean` are part of the writable
    # allowlist regardless of TRELLIS_CHECKER_SOCKET — the worker keeps
    # `.lake/build` writable for the inner edit-compile-fix loop.
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".trellis" / "runtime" / "rt1").mkdir(parents=True)
    (repo / ".lake" / "packages" / "mathlib").mkdir(parents=True)
    changed_paths = [
        repo / "Tablet" / "A.lean",
        worker_scratch_notes_path(repo),
        repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / "A.olean",
        repo / ".trellis" / "tmp" / "check" / "axioms_A_tmp.lean",
    ]

    violations = _repo_write_violations(repo, role="worker", changed_paths=changed_paths)

    assert violations == []


def test_certify_worker_checker_surface_reports_out_of_allowlist_write(
    tmp_path: Path,
    monkeypatch,
) -> None:
    repo = tmp_path / "repo"
    (repo / ".trellis" / "scripts").mkdir(parents=True)
    (repo / ".trellis" / "tmp").mkdir(parents=True)
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".trellis" / "scripts" / "check.py").write_text("", encoding="utf-8")

    def fake_sandbox_exec_command(**_: object):
        return ["python3", "-c", "pass"]

    def fake_run(cmd, capture_output, text):
        (repo / "Tablet.lean").write_text("-- rewritten", encoding="utf-8")
        class Result:
            returncode = 0
            stdout = ""
            stderr = ""
        return Result()

    monkeypatch.setattr("trellis.sandbox._sandbox_exec_command", fake_sandbox_exec_command)
    monkeypatch.setattr("trellis.sandbox.subprocess.run", fake_run)

    ok, detail = certify_worker_checker_surface(
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        repo_path=repo,
        burst_home=tmp_path,
    )

    assert not ok
    assert "Tablet.lean" in detail


def test_certify_worker_checker_surface_accepts_allowed_repo_writes(
    tmp_path: Path,
    monkeypatch,
) -> None:
    # Worker writes into `.lake/build/...A.olean` are always allowed (the
    # writable allowlist includes .lake/build regardless of TRELLIS_CHECKER_SOCKET).
    repo = tmp_path / "repo"
    (repo / ".trellis" / "scripts").mkdir(parents=True)
    (repo / ".trellis" / "tmp").mkdir(parents=True)
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".trellis" / "scripts" / "check.py").write_text("", encoding="utf-8")
    scratch_dir = worker_scratch_dir(repo)
    scratch_dir.mkdir(parents=True, exist_ok=True)
    scratch = worker_scratch_notes_path(repo)
    scratch.touch()
    (repo / ".lake" / "build").mkdir(parents=True, exist_ok=True)

    def fake_sandbox_exec_command(**_: object):
        return ["python3", "-c", "pass"]

    def fake_run(cmd, capture_output, text):
        (repo / "Tablet" / "A.lean").write_text("def A : Nat := 0\n", encoding="utf-8")
        scratch.write_text("notes\n", encoding="utf-8")
        checker_tmp = repo / ".trellis" / "tmp" / "check" / "probe"
        checker_tmp.parent.mkdir(parents=True, exist_ok=True)
        checker_tmp.write_text("tmp\n", encoding="utf-8")
        olean = repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / "A.olean"
        olean.parent.mkdir(parents=True, exist_ok=True)
        olean.write_bytes(b"compiled")
        class Result:
            returncode = 0
            stdout = ""
            stderr = ""
        return Result()

    monkeypatch.setattr("trellis.sandbox._sandbox_exec_command", fake_sandbox_exec_command)
    monkeypatch.setattr("trellis.sandbox.subprocess.run", fake_run)

    ok, detail = certify_worker_checker_surface(
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        repo_path=repo,
        burst_home=tmp_path,
    )

    assert ok
    assert detail == ""


def test_lake_compiler_bwrap_mounts_trellis_scripts_dir_readonly(
    tmp_path: Path, monkeypatch
) -> None:
    """Audit Fix 1: ``observe_lean_semantic_payloads`` runs
    ``lean --run <trellis_source>/scripts/lean_semantic_fingerprint.lean``;
    the lake_compiler bwrap must mount that scripts directory read-only or
    every supervisor-side semantic-payload call 404s.
    """
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    cmd = wrap_command(
        ["lake", "env", "lean", "--run", "scripts/lean_semantic_fingerprint.lean"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=tmp_path / "home",
        role="lake_compiler",
    )
    scripts_dir = _trellis_source_scripts_dir().resolve()
    assert _contains_bind(cmd, "--ro-bind", scripts_dir), (
        f"lake_compiler bwrap must mount {scripts_dir} ro so the semantic"
        f" fingerprint script resolves inside the bwrap"
    )
    # And it must not be a writable bind for that role.
    assert not _contains_bind(cmd, "--bind", scripts_dir), (
        f"scripts dir must stay read-only inside the lake_compiler bwrap"
    )


def test_lake_compiler_bwrap_argv_smoke(tmp_path: Path, monkeypatch) -> None:
    """Audit Fix 1 smoke: the lake_compiler bwrap argv builds without
    error and contains the load-bearing flags. Pins the structure so a
    future Step 2 refactor cannot accidentally drop ``--ro-bind``/``--bind``
    surfaces the supervisor-side checker server depends on.
    """
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".lake" / "build").mkdir(parents=True)

    cmd = wrap_command(
        ["lake", "build", "Tablet"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=tmp_path / "home",
        role="lake_compiler",
    )
    # bwrap is the launcher and the inner lake invocation survives untouched.
    assert cmd[0] == "bwrap"
    assert cmd[-3:] == ["lake", "build", "Tablet"]
    # ``--die-with-parent`` keeps the bwrap process tied to the supervisor
    # lifecycle; loss of this is a pid-leak class regression.
    assert "--die-with-parent" in cmd
    _assert_phase1_hardening(cmd)
    # Repo and Tablet source stay read-only; only build outputs are rebound
    # writable for the compiler.
    assert _contains_bind(cmd, "--ro-bind", repo)
    assert _contains_bind(cmd, "--bind", repo / ".lake" / "build")
    assert not _contains_bind(cmd, "--bind", repo / "Tablet")


def _setenv_value(cmd: list[str], name: str) -> str | None:
    """Return the value the bwrap command sets for env var ``name``,
    or ``None`` when no ``--setenv NAME VALUE`` triple is present."""
    for idx in range(len(cmd) - 2):
        if cmd[idx] == "--setenv" and cmd[idx + 1] == name:
            return cmd[idx + 2]
    return None


def test_worker_bwrap_wires_kernel_cache_when_runtime_root_set(
    tmp_path: Path, monkeypatch
) -> None:
    """Worker bwrap exposes the supervisor's kernel cache read-only and
    creates a per-runtime worker-writable cache, with both env vars set."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    runtime_root = tmp_path / "runtime"
    super_cache = runtime_root / "checker-state" / "kernel-cache"
    super_cache.mkdir(parents=True)
    monkeypatch.setenv("TRELLIS_KERNEL_CACHE_ROOT", str(runtime_root))

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )

    worker_cache = (runtime_root / "worker-cache").resolve()
    # Directory was created with the documented perms (0o770).
    assert worker_cache.is_dir()
    mode = worker_cache.stat().st_mode & 0o777
    assert mode == 0o770, f"expected 0o770, got {oct(mode)}"

    # Two binds present: writable worker cache + readonly supervisor cache.
    assert _contains_bind(cmd, "--bind", worker_cache)
    assert _contains_bind(cmd, "--ro-bind", super_cache.resolve())

    # Two setenvs: ROOT points at the worker's writable base, READONLY_ROOT
    # at the supervisor's runtime root (kernel binary appends the
    # standard `checker-state/kernel-cache/<ns>` subpath).
    assert _setenv_value(cmd, "TRELLIS_KERNEL_CACHE_ROOT") == str(worker_cache)
    assert _setenv_value(cmd, "TRELLIS_KERNEL_CACHE_READONLY_ROOT") == str(runtime_root.resolve())


def test_worker_bwrap_skips_cache_wiring_when_runtime_root_unset(
    tmp_path: Path, monkeypatch
) -> None:
    """No cache binds or env vars when ``TRELLIS_KERNEL_CACHE_ROOT`` is
    unset on the supervisor side (kernel disk cache disabled overall)."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.delenv("TRELLIS_KERNEL_CACHE_ROOT", raising=False)
    monkeypatch.delenv("TRELLIS_KERNEL_CACHE_READONLY_ROOT", raising=False)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )

    assert _setenv_value(cmd, "TRELLIS_KERNEL_CACHE_ROOT") is None
    assert _setenv_value(cmd, "TRELLIS_KERNEL_CACHE_READONLY_ROOT") is None


def test_lake_compiler_bwrap_does_not_wire_worker_kernel_cache(
    tmp_path: Path, monkeypatch
) -> None:
    """The supervisor-side ``lake_compiler`` bwrap does not consume the
    kernel disk cache (lake itself doesn't query it; the supervisor
    process that wraps lake does, but it sees the env var directly via
    its own process env, not via bwrap setenv). Confirm we don't redirect
    its ``TRELLIS_KERNEL_CACHE_ROOT`` to the worker cache base — that
    would corrupt supervisor writes by sending them into the
    worker-writable path."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    runtime_root = tmp_path / "runtime"
    (runtime_root / "checker-state" / "kernel-cache").mkdir(parents=True)
    monkeypatch.setenv("TRELLIS_KERNEL_CACHE_ROOT", str(runtime_root))

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    cmd = wrap_command(
        ["lake", "build"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/path/to/trellis"),
        role="lake_compiler",
    )

    # Neither cache var should be `--setenv`d in the lake_compiler bwrap.
    # (The supervisor process running lake_compiler still has these vars
    # in its OWN env — but we don't push them into the lake bwrap,
    # because lake doesn't query the kernel cache.)
    assert _setenv_value(cmd, "TRELLIS_KERNEL_CACHE_ROOT") is None
    assert _setenv_value(cmd, "TRELLIS_KERNEL_CACHE_READONLY_ROOT") is None


def test_reviewer_bwrap_does_not_wire_worker_kernel_cache(
    tmp_path: Path, monkeypatch
) -> None:
    """The reviewer role doesn't run lake or kernel observations — its
    bwrap shouldn't get the worker-cache wiring (no binds, no setenvs).
    The two-cache split is a worker-only concern."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    runtime_root = tmp_path / "runtime"
    (runtime_root / "checker-state" / "kernel-cache").mkdir(parents=True)
    monkeypatch.setenv("TRELLIS_KERNEL_CACHE_ROOT", str(runtime_root))

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    cmd = wrap_command(
        ["python3", "-c", "pass"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="reviewer",
    )

    assert _setenv_value(cmd, "TRELLIS_KERNEL_CACHE_ROOT") is None
    assert _setenv_value(cmd, "TRELLIS_KERNEL_CACHE_READONLY_ROOT") is None
    # And no bind for the worker cache base.
    assert not _contains_bind(cmd, "--bind", runtime_root / "worker-cache")


def test_worker_bwrap_skips_readonly_bind_when_supervisor_cache_absent(
    tmp_path: Path, monkeypatch
) -> None:
    """Cold-start case: ``TRELLIS_KERNEL_CACHE_ROOT`` is set on the
    supervisor side but the supervisor's cache directory hasn't been
    populated yet (fresh runtime, nothing cached). The worker bwrap
    should still wire up its writable cache (so workers can populate
    their own cache from scratch) but skip the readonly fallback bind +
    env (no entries to read yet, and binding a non-existent path would
    fail bwrap mount). Once the supervisor writes, *future* worker bwrap
    invocations will pick up the readonly side."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    # Intentionally do NOT create runtime_root / "checker-state" / "kernel-cache".
    monkeypatch.setenv("TRELLIS_KERNEL_CACHE_ROOT", str(runtime_root))

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )

    worker_cache = (runtime_root / "worker-cache").resolve()
    assert worker_cache.is_dir()
    # Writable side wired regardless of supervisor cache presence.
    assert _contains_bind(cmd, "--bind", worker_cache)
    assert _setenv_value(cmd, "TRELLIS_KERNEL_CACHE_ROOT") == str(worker_cache)
    # Readonly side skipped because the supervisor cache dir doesn't exist
    # yet — bwrap rejects --ro-bind of nonexistent paths.
    super_cache = runtime_root / "checker-state" / "kernel-cache"
    assert not _contains_bind(cmd, "--ro-bind", super_cache.resolve())
    assert _setenv_value(cmd, "TRELLIS_KERNEL_CACHE_READONLY_ROOT") is None


# --- Phase 4 bwrap-only sandbox migration tests ----------------------------


def test_burst_home_per_burst_seeding(tmp_path: Path, monkeypatch) -> None:
    """Phase 4: per-burst fake-home is materialized under
    `<runtime>/burst-homes/<burst_id>/` with hard-linked provider
    auth subdirs, bound into bwrap via `--bind`, exposed as `$HOME`,
    and removed at cleanup. Distinct bursts get distinct homes."""
    from trellis.burst_home import (
        burst_home_path,
        burst_homes_root,
        cleanup_burst_home,
        seed_burst_home,
    )

    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    # Stub source home with ~/.codex, ~/.claude, ~/.gemini.
    source_home = tmp_path / "source_home"
    for sub in (".codex", ".claude", ".gemini"):
        (source_home / sub).mkdir(parents=True)
        (source_home / sub / "auth.json").write_text("{}")
    (source_home / ".codex" / "sessions").mkdir()
    (source_home / ".codex" / "sessions" / "rollout-1.jsonl").write_text("{}\n")

    burst_id = "trellis-runtime-sound-1-v1"
    home = seed_burst_home(runtime_root, burst_id, source_home=source_home)

    # Layout assertions.
    assert home == burst_home_path(runtime_root, burst_id)
    assert home.is_dir()
    assert home.parent == burst_homes_root(runtime_root)
    for sub in (".codex", ".claude", ".gemini"):
        assert (home / sub / "auth.json").is_file()
    # Hard-link round-trip: same inode means provider auth refresh
    # writes propagate back to the supervisor's view automatically.
    assert (
        (home / ".codex" / "auth.json").stat().st_ino
        == (source_home / ".codex" / "auth.json").stat().st_ino
    )
    # The host's own rollouts are transcripts, not auth state: they are not
    # seeded (the burst writes its own under $HOME/.codex/sessions, and host
    # paths still resolve through the sandbox's absolute-path ro-bind).
    assert not (home / ".codex" / "sessions").exists()

    # Distinct burst_ids => distinct homes.
    other = seed_burst_home(
        runtime_root, "trellis-runtime-corr-1-v2", source_home=source_home
    )
    assert other != home
    assert other.exists()

    codex = seed_burst_home(runtime_root, "codex", source_home=source_home, provider="codex", persistent=True)
    assert (codex / ".codex/auth.json").is_file()
    assert not (codex / ".codex/sessions").exists()
    assert not (codex / ".claude").exists()
    (codex / ".codex/sessions").mkdir()
    own_session = codex / ".codex/sessions/own.jsonl"
    own_session.write_text("{}\n")
    seed_burst_home(runtime_root, "codex", source_home=source_home, provider="codex", persistent=True)
    assert own_session.read_text() == "{}\n"
    assert not (codex / ".codex/sessions/rollout-1.jsonl").exists()

    # wrap_command binds the fake-home and sets HOME to it, with no sudo.
    monkeypatch.setattr(Path, "home", lambda: source_home)
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=home,
        role="worker",
    )
    assert "sudo" not in cmd, (
        f"Phase 4: wrap_command must not emit 'sudo' tokens; got {cmd!r}"
    )
    assert _contains_bind(cmd, "--bind", home)
    for sub in (".codex", ".claude", ".gemini"):
        assert _contains_bind(cmd, "--ro-bind", source_home / sub)
    # --setenv HOME <home>
    found_home_setenv = False
    for i in range(len(cmd) - 2):
        if cmd[i] == "--setenv" and cmd[i + 1] == "HOME" and cmd[i + 2] == str(home):
            found_home_setenv = True
            break
    assert found_home_setenv, (
        f"Phase 4: --setenv HOME <fake-home> not in cmd: {cmd!r}"
    )

    # Cleanup removes the per-burst home, leaves peers untouched.
    cleanup_burst_home(home)
    assert not home.exists()
    assert other.exists()
    # Supervisor source home is untouched (hard-link refcount drop).
    assert (source_home / ".codex" / "auth.json").is_file()

    # Defense in depth: refuse paths outside burst-homes/.
    outside = tmp_path / "decoy"
    outside.mkdir()
    cleanup_burst_home(outside)
    assert outside.exists(), (
        "cleanup_burst_home must not recurse outside burst-homes/"
    )


def test_phase4_sandbox_exec_command_drops_sudo_prefix(tmp_path: Path, monkeypatch) -> None:
    """Phase 4: `_sandbox_exec_command` no longer emits the
    `sudo -n -u burst_user env HOME=...` outer wrap. The returned
    command is the plain bwrap argv."""
    from trellis.sandbox import _sandbox_exec_command

    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    cmd = _sandbox_exec_command(
        inner=["true"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        repo=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )
    assert cmd[0] == "bwrap"
    assert "sudo" not in cmd


# ---------------------------------------------------------------------
# Emission-shape invariants.
#
# Every `--setenv` assertion above this point is MEMBERSHIP-based
# (`("NAME", "value") in setenv_pairs`). That form cannot see a name
# emitted twice, which is how an accidental duplicate survived from merge
# 7549676c (2026-04-28) until 2026-07-31: two divergent branches each
# invented the value-passthrough mechanism, and their emission loops landed
# at different offsets in the `--setenv` block, so git auto-merged both.
#
# bwrap is last-wins, so the duplicate was invisible while both loops used
# the same dict — and became a silent security hole the moment anyone
# filtered one of them: the second emission re-asserted what the first had
# dropped. The tests below assert on COUNTS and MULTISETS so that class of
# defeat fails loudly.
# ---------------------------------------------------------------------


def _setenv_pairs(cmd):
    pairs = []
    for idx in range(len(cmd) - 2):
        if cmd[idx] == "--setenv":
            pairs.append((cmd[idx + 1], cmd[idx + 2]))
    return pairs


@pytest.mark.parametrize(
    "role", ["worker", "reviewer", "lake_compiler", "stuck_math_audit"]
)
def test_setenv_names_are_never_emitted_twice(tmp_path: Path, monkeypatch, role) -> None:
    """No env name may be emitted more than once, for any role.

    This is the invariant that would have caught the 2026-04-28 merge on
    the day it landed. It is role-parametrized because a duplicate is
    equally dangerous wherever it appears: it silently overrides any
    role-scoped filtering applied to the earlier emission.
    """
    from collections import Counter

    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.setenv("TRELLIS_CHECKER_SOCKET", "/tmp/trellis-checker.sock")
    monkeypatch.setenv("TRELLIS_CHECKER_TOKEN", "burst-token-abcdef")
    monkeypatch.setenv("TRELLIS_LEAN_PARALLELISM", "3")

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=tmp_path / "home",
        role=role,
    )
    counts = Counter(name for name, _ in _setenv_pairs(cmd))
    duplicated = sorted(name for name, n in counts.items() if n > 1)
    assert duplicated == [], f"role={role} emits duplicate --setenv names: {duplicated}"


def test_every_role_carries_its_sandbox_role_marker(tmp_path: Path, monkeypatch) -> None:
    """`TRELLIS_SANDBOX_ROLE` is the interior role signal. `trellis/checking.py`
    reads it to refuse the deterministic acceptance check for the roles whose
    bwrap withholds what that check needs, so the marker has to be on the argv
    for every role.
    """
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    for role in (
        "worker",
        "reviewer",
        "lake_compiler",
        "stuck_math_audit",
        "need_input_auditor",
        "revision_planner",
        "initial_planner",
        "gap_research",
        "gap_plan_critic",
        "assumptions_lane",
    ):
        cmd = wrap_command(
            ["echo", "ok"],
            sandbox=SandboxConfig(enabled=True, backend="bwrap"),
            work_dir=repo,
            burst_home=tmp_path / "home",
            role=role,
        )
        assert ["--setenv", "TRELLIS_SANDBOX_ROLE", role] in [
            cmd[idx : idx + 3] for idx in range(len(cmd) - 2)
        ], f"role={role}: bwrap argv must carry the interior role marker"


def _bind_index(cmd: list[str], mode: str, path: Path) -> int:
    for i in range(len(cmd) - 2):
        if cmd[i] == mode and cmd[i + 1] == str(path) and cmd[i + 2] == str(path):
            return i
    raise AssertionError(f"{mode} {path} not in argv")


def test_burst_home_nested_in_read_only_work_dir_is_bound_after_it(tmp_path: Path, monkeypatch) -> None:
    """Phase 0 keeps the burst home inside the read-only attempt root; the
    writable home bind must come after the root's ro-bind or bwrap shadows it
    and codex exits with EROFS before the agent starts."""
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)
    monkeypatch.delenv("TRELLIS_PHASE0_CHECKER_SOCKET", raising=False)
    attempt = tmp_path / "attempt"
    home = attempt / "burst-homes" / "worker"
    home.mkdir(parents=True)
    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=attempt,
        burst_home=home,
        role="source_adaptation_worker",
    )
    assert _bind_index(cmd, "--ro-bind", attempt) < _bind_index(cmd, "--bind", home)
    assert _bind_index(cmd, "--ro-bind", attempt) < _bind_index(cmd, "--bind", attempt / "candidate")
    assert _bind_index(cmd, "--ro-bind", attempt) < _bind_index(cmd, "--bind", attempt / ".trellis" / "chats")


def test_burst_home_outside_work_dir_keeps_home_before_repo(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    home = tmp_path / "runtime" / "burst-homes" / "worker"
    home.mkdir(parents=True)
    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=home,
        role="worker",
    )
    assert _bind_index(cmd, "--bind", home) < _bind_index(cmd, "--ro-bind", repo)


def test_provider_state_private_subtrees_are_masked(tmp_path: Path, monkeypatch) -> None:
    """The absolute-path ro-bind of ~/.claude must not expose the operator's
    session transcripts and job scratch; those subtrees are masked with tmpfs."""
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)
    fake_home = tmp_path / "home"
    for sub in ("jobs", "projects", "plugins"):
        (fake_home / ".claude" / sub).mkdir(parents=True)
    (fake_home / ".claude" / ".credentials.json").write_text("{}")
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    burst_home = tmp_path / "runtime" / "burst-homes" / "worker"
    burst_home.mkdir(parents=True)
    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=burst_home,
        role="worker",
    )
    claude = fake_home / ".claude"
    ro = _bind_index(cmd, "--ro-bind", claude)
    for sub in ("jobs", "projects"):
        idx = cmd.index("--tmpfs", ro)
        while cmd[idx + 1] != str(claude / sub):
            idx = cmd.index("--tmpfs", idx + 1)
        assert idx > ro
    assert ["--tmpfs", str(claude / "plugins")] not in [cmd[i:i + 2] for i in range(len(cmd) - 1)]


def test_host_codex_rollouts_are_masked_in_bursts(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)
    fake_home = tmp_path / "home"
    (fake_home / ".codex" / "sessions" / "2026").mkdir(parents=True)
    (fake_home / ".codex" / "auth.json").write_text("{}")
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    burst_home = tmp_path / "runtime" / "burst-homes" / "worker"
    burst_home.mkdir(parents=True)
    cmd = wrap_command(
        ["echo", "ok"], sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo, burst_home=burst_home, role="worker",
    )
    codex = fake_home / ".codex"
    assert _bind_index(cmd, "--ro-bind", codex) < cmd.index(str(codex / "sessions")) - 1
    assert cmd[cmd.index(str(codex / "sessions")) - 1] == "--tmpfs"


def test_phase0_worker_cargo_target_dir_is_outside_the_candidate(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)
    monkeypatch.delenv("TRELLIS_PHASE0_CHECKER_SOCKET", raising=False)
    attempt = tmp_path / "attempt"
    home = attempt / "burst-homes" / "worker"
    home.mkdir(parents=True)
    cmd = wrap_command(
        ["echo", "ok"], sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=attempt, burst_home=home, role="source_adaptation_worker",
    )
    i = cmd.index("CARGO_TARGET_DIR")
    assert cmd[i - 1] == "--setenv"
    target = Path(cmd[i + 1])
    assert target == home / ".trellis-cargo-target"
    assert not target.is_relative_to(attempt / "candidate")
