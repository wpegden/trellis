from __future__ import annotations

from pathlib import Path

from trellis.host_runtime import (
    DEFAULT_WORKER_PATH,
    _claude_runtime_root,
    _nvm_version_root,
    host_runtime_readonly_roots,
    provider_cli_not_found_detail,
    resolved_command_bin_dir,
    worker_path_env,
    worker_provider_bin_dirs,
)


def test_nvm_version_root_detection() -> None:
    resolved = Path(
        "${TRELLIS_ROOT:-/path/to/trellis}/.nvm/versions/node/v22.22.2/lib/node_modules/@openai/codex/bin/codex.js"
    )
    assert _nvm_version_root(resolved) == Path("${TRELLIS_ROOT:-/path/to/trellis}/.nvm/versions/node/v22.22.2")


def test_claude_runtime_root_detection() -> None:
    resolved = Path("${TRELLIS_ROOT:-/path/to/trellis}/.local/share/claude/versions/2.1.97")
    assert _claude_runtime_root(resolved) == Path("${TRELLIS_ROOT:-/path/to/trellis}/.local/share/claude")


def test_host_runtime_roots_include_elan(monkeypatch, tmp_path: Path) -> None:
    elan_home = tmp_path / ".elan"
    elan_home.mkdir(parents=True)
    monkeypatch.setenv("ELAN_HOME", str(elan_home))
    roots = host_runtime_readonly_roots(include_tools=())
    assert roots == [elan_home.resolve()]


def _make_fake_cli(bin_dir: Path, name: str) -> Path:
    bin_dir.mkdir(parents=True, exist_ok=True)
    exe = bin_dir / name
    exe.write_text("#!/bin/sh\n")
    exe.chmod(0o755)
    return exe


def test_resolved_command_bin_dir_prefers_burst_user(tmp_path: Path) -> None:
    # GATE H: a per-burst npm-global install is resolved against the burst home.
    burst_home = tmp_path / "home"
    bin_dir = burst_home / ".local" / "share" / "npm-global" / "bin"
    _make_fake_cli(bin_dir, "codex")
    assert resolved_command_bin_dir("codex", burst_home=burst_home) == bin_dir


def test_resolved_command_bin_dir_uses_which(monkeypatch, tmp_path: Path) -> None:
    # GATE H: with no burst-user copy, fall through to the launcher's dir on
    # the supervisor PATH — the same dir the read-only binds derive from.
    bin_dir = tmp_path / "tools"
    _make_fake_cli(bin_dir, "gemini")
    monkeypatch.setattr(
        "trellis.host_runtime.shutil.which",
        lambda n: str(bin_dir / n) if n == "gemini" else None,
    )
    assert resolved_command_bin_dir("gemini", burst_home=None) == bin_dir


def test_worker_path_env_includes_resolved_cli_dir_additive(
    monkeypatch, tmp_path: Path,
) -> None:
    # GATE H: the resolved provider-CLI dir lands on PATH, and every entry of
    # the previous (base) value is preserved in order (additive).
    burst_home = tmp_path / "home"
    burst_home.mkdir()
    nvm_bin = tmp_path / "nvm" / "bin"
    _make_fake_cli(nvm_bin, "codex")
    monkeypatch.setattr(
        "trellis.host_runtime.shutil.which",
        lambda n: str(nvm_bin / n) if n == "codex" else None,
    )

    path = worker_path_env(burst_home)
    entries = path.split(":")
    assert str(nvm_bin) in entries
    # Base entries still present, in their original relative order.
    base = [
        f"{burst_home}/.trellis-npm/bin",
        f"{burst_home}/.local/share/npm-global/bin",
        *DEFAULT_WORKER_PATH.split(":"),
    ]
    idxs = [entries.index(e) for e in base]
    assert idxs == sorted(idxs)
    # Resolved CLI dir is prepended ahead of the base entries.
    assert entries.index(str(nvm_bin)) < entries.index(base[0])


def test_worker_path_env_none_is_default() -> None:
    assert worker_path_env(None) == DEFAULT_WORKER_PATH


def test_worker_provider_bin_dirs_skips_missing(monkeypatch, tmp_path: Path) -> None:
    bin_dir = tmp_path / "tools"
    _make_fake_cli(bin_dir, "claude")
    monkeypatch.setattr(
        "trellis.host_runtime.shutil.which",
        lambda n: str(bin_dir / n) if n == "claude" else None,
    )
    dirs = worker_provider_bin_dirs(burst_home=None)
    assert dirs == [bin_dir]  # only the one that exists


def test_provider_cli_not_found_detail_on_127() -> None:
    msg = provider_cli_not_found_detail(
        "codex", exit_code=127, output="", burst_home=None,
    )
    assert msg is not None
    assert "codex" in msg and "PATH" in msg


def test_provider_cli_not_found_detail_on_command_not_found() -> None:
    msg = provider_cli_not_found_detail(
        "claude", exit_code=1, output="claude: command not found", burst_home=None,
    )
    assert msg is not None and "claude" in msg


def test_provider_cli_not_found_detail_ignores_real_error() -> None:
    # A genuine non-127 failure must NOT be relabeled — retry semantics for
    # real model/agent errors stay untouched.
    assert provider_cli_not_found_detail(
        "codex", exit_code=2, output="model returned an error", burst_home=None,
    ) is None
