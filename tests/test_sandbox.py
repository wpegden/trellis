from __future__ import annotations

from pathlib import Path

from trellis.config import SandboxConfig
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


def test_worker_sandbox_binds_checker_socket_dir_when_env_var_set(
    tmp_path: Path, monkeypatch
) -> None:
    """Bug fix: when TRELLIS_CHECKER_SOCKET is set, the worker bwrap must
    bind the socket's parent directory read-only so the worker burst's
    check.py can ``connect()`` to the supervisor-side unified-checker
    UNIX socket. Forwarding the env var via ``--setenv`` alone is not
    enough — bwrap strips the host filesystem by default, so the
    socket file path resolves to nothing inside the sandbox and connect()
    fails with FileNotFoundError, surfaced as a
    ``supervisor_unavailable`` RPC error."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    runtime_root = tmp_path / "example-run-runtime"
    socket_dir = runtime_root / "sockets"
    socket_dir.mkdir(parents=True)
    socket_path = socket_dir / "checker.sock"
    monkeypatch.setenv("TRELLIS_CHECKER_SOCKET", str(socket_path))

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )

    # The socket directory is bound read-only so connect() can reach the
    # socket node inside the sandbox. Read-only is sufficient — the
    # supervisor owns the socket file; the worker only needs to connect.
    assert _contains_bind(cmd, "--ro-bind", socket_dir.resolve())
    # Must not be a writable bind: the worker has no business creating
    # or replacing the socket file.
    assert not _contains_bind(cmd, "--bind", socket_dir.resolve())
    # The env var is still forwarded so the worker's check.py can resolve
    # the socket path.
    setenv_pairs = []
    for idx in range(len(cmd) - 2):
        if cmd[idx] == "--setenv":
            setenv_pairs.append((cmd[idx + 1], cmd[idx + 2]))
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


def test_lake_compiler_sandbox_keeps_full_writable_set_in_rpc_mode(
    tmp_path: Path, monkeypatch
) -> None:
    """Step 5: the lake_compiler role is the supervisor-side bwrap that
    runs lake on behalf of the unified-checker RPC server. Setting
    TRELLIS_CHECKER_SOCKET must NOT shrink its allowlist — it still needs
    `.lake/build`, per-package `.lake/build`, `Tablet/`, and the print_axioms
    scratch dirs writable so observation calls can land their outputs.
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
    assert _contains_bind(cmd, "--bind", repo / "Tablet")
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "tmp")
    assert _contains_bind(cmd, "--bind", repo / ".trellis" / "staging")


def test_lake_compiler_sandbox_unaffected_when_checker_socket_unset(
    tmp_path: Path, monkeypatch
) -> None:
    """Step 5 sanity: the env-var gate only changes the worker branch.
    Pin lake_compiler's writable set with the env unset so a future
    refactor can't accidentally extend the gate to the supervisor side.
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
    assert _contains_bind(cmd, "--bind", repo / "Tablet")


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
    # Repo must be ro-bind for lake_compiler (Tablet writes go through the
    # narrow allowlist in _repo_writable_paths).
    assert _contains_bind(cmd, "--ro-bind", repo)
    assert _contains_bind(cmd, "--bind", repo / ".lake" / "build")
    assert _contains_bind(cmd, "--bind", repo / "Tablet")


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
        burst_home=Path("${TRELLIS_ROOT:-/path/to/trellis}"),
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
    assert (
        (home / ".codex" / "sessions" / "rollout-1.jsonl").stat().st_ino
        == (source_home / ".codex" / "sessions" / "rollout-1.jsonl").stat().st_ino
    )

    # Distinct burst_ids => distinct homes.
    other = seed_burst_home(
        runtime_root, "trellis-runtime-corr-1-v2", source_home=source_home
    )
    assert other != home
    assert other.exists()

    # wrap_command binds the fake-home and sets HOME to it, with no sudo.
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
