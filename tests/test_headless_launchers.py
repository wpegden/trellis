from __future__ import annotations

from pathlib import Path

from trellis.adapters import ProviderConfig
from trellis.agents import codex_headless
from trellis.agents.codex_headless import build_launcher_script as build_codex_launcher_script
from trellis.agents.script_headless import (
    _gemini_capacity_loop_should_abort,
    build_launcher_script as build_script_launcher_script,
)


def _assert_launcher_script(path: Path, *, expected_first_command: str) -> None:
    assert path.exists()
    assert path.stat().st_mode & 0o111
    text = path.read_text(encoding="utf-8")
    assert text.startswith("#!/usr/bin/env bash\nset -euo pipefail\nexec \\\n")
    # Post-bwrap-only: no sudo wrap; the first exec'd command is the bwrap.
    assert "  sudo " not in text
    assert f"  {expected_first_command} \\\n" in text


def test_codex_launcher_script_uses_short_executable_wrapper(tmp_path: Path) -> None:
    path = build_codex_launcher_script(
        script_path=tmp_path / "burst.sh",
        launch_cmd=["bwrap", "--chdir", "/repo", "/repo/burst.sh"],
        log_dir=tmp_path,
        log_prefix="worker",
    )
    _assert_launcher_script(path, expected_first_command="bwrap")
    text = path.read_text(encoding="utf-8")
    assert "  /repo/burst.sh\n" in text


def test_script_launcher_script_uses_short_executable_wrapper(tmp_path: Path) -> None:
    path = build_script_launcher_script(
        launch_cmd=["bwrap", "--chdir", "/repo", "/repo/burst.sh"],
        log_dir=tmp_path,
        log_prefix="review",
    )
    _assert_launcher_script(path, expected_first_command="bwrap")
    text = path.read_text(encoding="utf-8")
    assert "  /repo/burst.sh\n" in text


def test_gemini_capacity_loop_abort_detects_repeated_startup_429s() -> None:
    output = """YOLO mode is enabled. All tool calls will be automatically approved.
Loaded cached credentials.
Attempt 1 failed with status 429. Retrying with backoff...
"reason": "MODEL_CAPACITY_EXHAUSTED"
"model": "gemini-3.1-pro-preview"
Attempt 2 failed with status 429. Retrying with backoff...
"reason": "MODEL_CAPACITY_EXHAUSTED"
"model": "gemini-3.1-pro-preview"
Attempt 3 failed with status 429. Retrying with backoff...
"reason": "MODEL_CAPACITY_EXHAUSTED"
"model": "gemini-3.1-pro-preview"
"""
    assert _gemini_capacity_loop_should_abort(output, "gemini-3.1-pro-preview") is True


def test_gemini_capacity_loop_abort_does_not_fire_after_substantive_work() -> None:
    output = """YOLO mode is enabled. All tool calls will be automatically approved.
Loaded cached credentials.
Attempt 1 failed with status 429. Retrying with backoff...
"reason": "MODEL_CAPACITY_EXHAUSTED"
"model": "gemini-3.1-pro-preview"
Bash command parsing error detected for command: << 'EOF' > Tablet/test.lean
Attempt 2 failed with status 429. Retrying with backoff...
"reason": "MODEL_CAPACITY_EXHAUSTED"
"model": "gemini-3.1-pro-preview"
Attempt 3 failed with status 429. Retrying with backoff...
"reason": "MODEL_CAPACITY_EXHAUSTED"
"model": "gemini-3.1-pro-preview"
"""
    assert _gemini_capacity_loop_should_abort(output, "gemini-3.1-pro-preview") is False


def test_codex_session_state_path_is_partitioned_by_scope_and_model(tmp_path: Path) -> None:
    theorem_path = codex_headless._session_state_path(
        tmp_path,
        "worker",
        "theorem_stating:worker:theorem:codex:gpt-5.4:xhigh",
        ProviderConfig(provider="codex", model="gpt-5.4", effort="xhigh"),
    )
    proof_path = codex_headless._session_state_path(
        tmp_path,
        "worker",
        "proof_formalization:worker:proof_hard:codex:gpt-5.4:xhigh",
        ProviderConfig(provider="codex", model="gpt-5.4", effort="xhigh"),
    )

    assert theorem_path != proof_path
