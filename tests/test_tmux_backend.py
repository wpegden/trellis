"""Unit tests for parsers + detectors in trellis.agents.tmux_backend.

No real tmux/agent dependency. Run with:
    python3 tests/test_tmux_backend.py
or under pytest:
    python3 -m pytest tests/test_tmux_backend.py -v
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path

from trellis.agents.tmux_backend import (  # noqa: E402
    strip_ansi,
    strip_spinner_chars,
    normalize_pane,
    agent_is_busy,
    agent_has_terminal_api_error,
    agent_requires_auth,
    _claude_input_line_is_empty,
    _gemini_input_line_is_empty,
    _detect_dialog,
    detect_gemini_permission_prompt,
    is_rate_limited,
    is_fast_retryable,
    extract_exhausted_model,
    exponential_backoff,
    session_name,
    _session_identity_payload,
    identities_match,
    store_session_identity,
    load_session_identity,
    clear_session_identity,
    claude_cost_usd,
    _pids_using_claude_session_id,
    _claude_session_id_in_use,
    append_cost_ledger,
    summarize_cost_ledger,
    default_cost_ledger_path,
    per_lane_tmpdir,
)


# -------------- results / harness --------------

_RESULTS: list[tuple[str, bool, str]] = []


def run(name: str, fn):
    try:
        fn()
        _RESULTS.append((name, True, ""))
    except AssertionError as exc:
        _RESULTS.append((name, False, f"AssertionError: {exc}"))
    except Exception as exc:
        _RESULTS.append((name, False, f"{type(exc).__name__}: {exc}"))


# -------------- tests --------------

def test_strip_ansi_basic():
    assert strip_ansi("\x1b[31mRED\x1b[0m ok") == "RED ok"
    assert strip_ansi("\x1b]0;title\x07prompt") == "prompt"
    assert strip_ansi("plain") == "plain"


def test_strip_spinner_chars():
    assert strip_spinner_chars("⠙ ⠹ ⠸ Thinking...") == "   Thinking..."
    assert strip_spinner_chars("|/-\\ spinning") == " spinning"
    assert strip_spinner_chars("✦ ok") == " ok"
    # Non-spinner unicode should survive.
    assert "日本" in strip_spinner_chars("日本 ⠙ text")


def test_normalize_pane_trailing():
    raw = "\x1b[32mhello\x1b[0m world   \n\n\n"
    assert normalize_pane(raw) == "hello world"


def test_agent_is_busy_claude():
    assert agent_is_busy("… esc to interrupt …", "claude") is True
    assert agent_is_busy("ctrl-c to interrupt", "claude") is True
    assert agent_is_busy("normal prompt state", "claude") is False


def test_agent_is_busy_gemini_default():
    # Confirmed on-screen text during gemini busy in default UI.
    assert agent_is_busy("⠙ Thinking... (esc to cancel, 0s)", "gemini") is True
    assert agent_is_busy("Responding...", "gemini") is True
    assert agent_is_busy("idle screen", "gemini") is False


def test_agent_is_busy_gemini_screenreader():
    # Confirmed on-screen text during gemini busy in --screen-reader mode.
    assert agent_is_busy("responding Thinking... (esc to cancel, 0s) ? for shortcuts", "gemini") is True


def test_agent_requires_auth():
    assert agent_requires_auth("Please log in to Claude\n", "claude") is True
    assert agent_requires_auth("Please authenticate\n", "claude") is True
    assert agent_requires_auth("Authentication required\n", "claude") is True
    assert agent_requires_auth("Working normally", "claude") is False
    assert agent_requires_auth("Please log in to Gemini", "gemini") is True
    assert agent_requires_auth("Google sign-in required", "gemini") is True


def test_claude_input_empty_heuristic():
    pane = """
─────────────
❯
─────────────
⏵⏵ bypass permissions on (shift+tab to cycle)
""".strip()
    assert _claude_input_line_is_empty(pane) is True
    # With text typed.
    filled = """
─────────────
❯ hello world
─────────────
""".strip()
    assert _claude_input_line_is_empty(filled) is False


def test_gemini_input_empty_heuristic():
    # Default mode: has box borders + placeholder.
    pane = """
▀▀▀▀▀▀▀▀▀▀▀▀▀▀
 *   Type your message or @path/to/file
▄▄▄▄▄▄▄▄▄▄▄▄▄▄
"""
    assert _gemini_input_line_is_empty(pane) is True
    # Screen-reader mode: placeholder present, no box.
    sr = "Type your message or @path/to/file\n? for shortcuts"
    assert _gemini_input_line_is_empty(sr) is True
    # Content typed should NOT look empty.
    filled = """
▀▀▀▀▀▀▀▀▀▀▀▀▀▀
 > hello there
▄▄▄▄▄▄▄▄▄▄▄▄▄▄
"""
    assert _gemini_input_line_is_empty(filled) is False


def test_detect_dialog_claude():
    d = _detect_dialog("claude", "Quick safety check: Is this a project you created or one you trust?")
    assert d is not None and "Is this a project" in d[0]
    assert _detect_dialog("claude", "normal prompt") is None


def test_detect_dialog_gemini():
    d = _detect_dialog("gemini", "Do you trust the files in this folder?")
    assert d is not None and "Do you trust" in d[0]


def test_detect_gemini_permission_prompt_real_pane():
    fixture = (Path(__file__).parent / "fixtures"
               / "gemini-permission-prompts"
               / "20260424-111230-rm-permission-plain.txt").read_text()
    assert detect_gemini_permission_prompt(fixture) == "rm"


def test_detect_gemini_permission_prompt_negative():
    # Plain output, no prompt.
    assert detect_gemini_permission_prompt("just some output\nno prompt here") is None
    # "Allow execution" line present but no choice list following.
    assert detect_gemini_permission_prompt(
        "Allow execution of [rm]?\nfoo\nbar\nbaz\nqux\nquux\n"
    ) is None
    # Choice list within window but missing the "Allow" word.
    assert detect_gemini_permission_prompt(
        "Allow execution of [rm]?\n1. Skip\n2. Foo\n3. Bar\n"
    ) is None


def test_detect_gemini_permission_prompt_synthetic():
    # Distilled minimal positive case — single trailing-newline.
    minimal = "Allow execution of [lake]?\n(checked) 1. Allow once\n"
    assert detect_gemini_permission_prompt(minimal) == "lake"
    # Different command name.
    assert detect_gemini_permission_prompt(
        "Allow execution of [git push]?\n1. Allow once\n2. Allow for this session\n"
    ) == "git push"


def test_rate_limit_patterns():
    assert is_rate_limited("Rate limited: wait")
    assert is_rate_limited("Error 429: Too Many Requests")
    assert is_rate_limited("RESOURCE_EXHAUSTED")
    assert is_rate_limited("Model_capacity_exhausted for claude-opus-4-7")
    assert is_rate_limited("Credit balance is too low") is True
    assert is_rate_limited("OK") is False
    assert is_rate_limited("") is False


def test_fast_retryable():
    assert is_fast_retryable("agent died immediately after receiving prompt") is True
    assert is_fast_retryable("Network ok") is False


def test_extract_exhausted_model():
    assert extract_exhausted_model("No capacity available for model gemini-3-flash-preview") == "gemini-3-flash-preview"
    assert extract_exhausted_model('MODEL_CAPACITY_EXHAUSTED: {"model": "claude-opus-4-7"}') == "claude-opus-4-7"
    assert extract_exhausted_model("normal error text") is None


def test_exponential_backoff():
    # Matches trellis.burst pattern: base * 2**attempt, capped at max_delay.
    assert exponential_backoff(base=30, attempt=0, max_delay=900) == 30
    assert exponential_backoff(base=30, attempt=1, max_delay=900) == 60
    assert exponential_backoff(base=30, attempt=5, max_delay=900) == 900  # capped
    assert exponential_backoff(base=60, attempt=0, max_delay=120) == 60
    assert exponential_backoff(base=60, attempt=2, max_delay=120) == 120  # capped


def test_session_name_slug():
    assert session_name("claude", "worker") == "trellis-claude-worker"
    assert session_name("gemini", "verifier", session_scope="corr/v1") == "trellis-gemini-verifier-corr-v1"
    assert session_name("claude", "reviewer", session_scope="theorem:proof:1") == "trellis-claude-reviewer-theorem-proof-1"
    assert session_name("claude", "worker", session_scope="scope with spaces", extra="retry") == "trellis-claude-worker-scope-with-spaces-retry"


def test_identity_sidecar_roundtrip():
    with tempfile.TemporaryDirectory() as td:
        cwd = Path(td)
        assert load_session_identity(cwd, provider="claude", role="worker", session_scope="s") is None
        id1 = _session_identity_payload(provider="claude", model="opus", effort="max", session_scope="s")
        store_session_identity(cwd, provider="claude", role="worker", session_scope="s", session_id="sidA", identity=id1)
        got = load_session_identity(cwd, provider="claude", role="worker", session_scope="s")
        assert got["session_id"] == "sidA"
        assert identities_match(got["identity"], id1)
        id2 = _session_identity_payload(provider="claude", model="opus", effort="low", session_scope="s")
        assert not identities_match(id1, id2)
        clear_session_identity(cwd, provider="claude", role="worker", session_scope="s")
        assert load_session_identity(cwd, provider="claude", role="worker", session_scope="s") is None


def test_claude_cost_usd():
    usage = {
        "model": "claude-opus-4-7",
        "input_tokens": 1000,
        "output_tokens": 500,
        "cache_read_input_tokens": 0,
        "cache_creation_input_tokens": 0,
    }
    cost = claude_cost_usd(usage)
    # 1000 * 15 + 500 * 75 = 15000 + 37500 = 52500 / 1M = 0.0525
    assert abs(cost - 0.0525) < 1e-6, cost
    # Unknown model → None.
    assert claude_cost_usd({"model": "claude-unknown-9-9", "input_tokens": 100, "output_tokens": 50}) is None


def test_cost_ledger_roundtrip():
    with tempfile.TemporaryDirectory() as td:
        cwd = Path(td)
        for i in range(3):
            append_cost_ledger(
                cwd, provider="claude", role="worker", scope="theorem", model="claude-opus-4-7",
                usage={"input_tokens": 10, "output_tokens": 20, "model": "claude-opus-4-7"},
                cost_usd=0.01, duration_seconds=5.0, attempts=1, ok=True, reason="stable",
            )
        summary = summarize_cost_ledger(default_cost_ledger_path(cwd))
        assert summary["bursts"] == 3
        assert abs(summary["grand"]["cost_usd"] - 0.03) < 1e-6
        assert summary["totals"][0]["provider"] == "claude"
        assert summary["totals"][0]["count"] == 3
        assert summary["totals"][0]["ok_count"] == 3


def test_cost_ledger_cumulative_session_delta():
    """When cumulative_cost_usd is passed across a resumed session, the ledger
    records per-burst deltas in cost_usd and tokens, so summarize's totals
    match the session's final cumulative — not 4x inflated by re-summing.
    """
    with tempfile.TemporaryDirectory() as td:
        cwd = Path(td)
        # Simulate 3 bursts of a resumed claude session whose transcript
        # usage grows cumulatively: 0.10 -> 0.25 -> 0.40 USD, with tokens
        # also monotonically rising.
        cumulatives = [
            (0.10, {"input_tokens": 1000, "output_tokens": 500,
                    "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
            (0.25, {"input_tokens": 2500, "output_tokens": 1200,
                    "cache_read_input_tokens": 300, "cache_creation_input_tokens": 100}),
            (0.40, {"input_tokens": 4000, "output_tokens": 2000,
                    "cache_read_input_tokens": 500, "cache_creation_input_tokens": 200}),
        ]
        for cum_cost, cum_usage in cumulatives:
            append_cost_ledger(
                cwd, provider="claude", role="worker", scope="theorem",
                model="claude-opus-4-7",
                usage=cum_usage,
                cost_usd=None,
                cumulative_cost_usd=cum_cost,
                duration_seconds=5.0, attempts=1, ok=True, reason="stable",
                session_id="sess-resumed",
            )
        summary = summarize_cost_ledger(default_cost_ledger_path(cwd))
        # Sum of per-burst deltas should equal the final cumulative.
        assert abs(summary["grand"]["cost_usd"] - 0.40) < 1e-6, summary["grand"]
        t = summary["totals"][0]
        assert t["input_tokens"] == 4000, t
        assert t["output_tokens"] == 2000, t
        assert t["cache_read"] == 500, t
        assert t["cache_write"] == 200, t


def test_cost_ledger_cumulative_cross_session_independence():
    """Two separate sessions' cumulatives must not cancel each other."""
    with tempfile.TemporaryDirectory() as td:
        cwd = Path(td)
        for cum_cost in (0.05, 0.15):
            append_cost_ledger(
                cwd, provider="claude", role="worker", scope="theorem",
                model="claude-opus-4-7",
                usage={"input_tokens": 100, "output_tokens": 50,
                       "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
                cost_usd=None, cumulative_cost_usd=cum_cost,
                duration_seconds=1.0, attempts=1, ok=True, reason="stable",
                session_id="sess-A",
            )
        for cum_cost in (0.03, 0.07):
            append_cost_ledger(
                cwd, provider="claude", role="worker", scope="theorem",
                model="claude-opus-4-7",
                usage={"input_tokens": 100, "output_tokens": 50,
                       "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
                cost_usd=None, cumulative_cost_usd=cum_cost,
                duration_seconds=1.0, attempts=1, ok=True, reason="stable",
                session_id="sess-B",
            )
        summary = summarize_cost_ledger(default_cost_ledger_path(cwd))
        # Final cumulatives: A=0.15, B=0.07, grand=0.22.
        assert abs(summary["grand"]["cost_usd"] - 0.22) < 1e-6, summary["grand"]


def test_per_lane_tmpdir_creates_and_isolates():
    p1 = per_lane_tmpdir("ut-lane-A")
    p2 = per_lane_tmpdir("ut-lane-B")
    assert p1 != p2
    assert p1.is_dir() and p2.is_dir()
    # Writeability.
    (p1 / "probe").write_text("ok", encoding="utf-8")
    assert (p1 / "probe").read_text(encoding="utf-8") == "ok"


def test_claude_session_id_in_use_detector():
    screen = "... Session ID abc123 already in use by another process ..."
    assert _claude_session_id_in_use(screen) is True
    assert _claude_session_id_in_use("normal screen") is False


def test_pids_using_claude_session_id_self():
    # Our own /proc/self/cmdline should NOT match a random uuid.
    assert _pids_using_claude_session_id("00000000-0000-0000-0000-000000000000") == []


# ---- apparent_stall liveness uses POSITIVE work signals only ---------------
# Background: Worker/98 on BinomialCoefficientAsymptotics was killed 3 times
# in a row at apparent_stall_1200s because the old detector used
# normalize_pane (which strips timer/spinner/busy-line) as its liveness
# signal — long pure-reasoning phases normalized to a constant string even
# though the agent was clearly alive. We briefly tried raw-pane tick as the
# signal, but a TUI-render ticker is not proof the *reasoning thread* is
# alive (async event loops can tick while the reasoning task is wedged).
# The current design uses only unforgeable positive work signals:
#   (1) workspace_paths: tool-call FS writes.
#   (2) liveness_probe: session-transcript file mtime (agent process has
#       to actually call write() to advance it; TUI threads cannot).
# Either one firing resets last_liveness_ns; apparent_stall fires only when
# BOTH have been stale for the whole window.

def test_send_prompt_resends_enter_when_first_keypress_is_swallowed(tmp_path: Path):
    """send_prompt must verify the agent became busy after Enter; if a busy
    marker doesn't appear within the verify window, re-send Enter (the
    first keypress can be swallowed by the paste-rendering pipeline of
    gemini's TUI for large prompts). Worker 158 on canary cycle 50 sat
    idle for 20 minutes because the supervisor's 0.2s sleep + single
    Enter wasn't enough — the prompt remained pasted but never submitted."""
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    enter_attempts = [0]
    busy_marker_after_attempts = 2  # not busy on attempt 1 — busy on attempt 2

    busy_screen = "responding Thinking... (esc to cancel, 0m 1s) ? for shortcuts\n"
    idle_screen = "YOLO mode  Type your message or @path/to/file\n"

    tmux_calls = []
    def fake_tmux(*args, **kwargs):
        tmux_calls.append(args)
        # If this call is `send-keys ... Enter`, count it
        if "Enter" in args:
            enter_attempts[0] += 1
        class FakeResult:
            returncode = 0
            stdout = ""
            stderr = ""
        return FakeResult()

    def fake_capture(_session, history=False):
        # Return busy_screen only after the configured number of Enter attempts
        return busy_screen if enter_attempts[0] >= busy_marker_after_attempts else idle_screen

    sleep_calls = []
    def fake_sleep(dt):
        sleep_calls.append(dt)

    now_state = [0.0]
    def fake_monotonic():
        now_state[0] += 0.1
        return now_state[0]

    class FakeHandle:
        session = "fake-session"
        cwd = Path("/tmp")
        provider = "gemini"
        session_id = "fake-sid"

    with (
        patch.object(tmb, "tmux", fake_tmux),
        patch.object(tmb, "capture", fake_capture),
        patch.object(tmb.time, "sleep", fake_sleep),
        patch.object(tmb.time, "monotonic", fake_monotonic),
    ):
        tmb.send_prompt(FakeHandle(), "small prompt", pre_enter_settle=0.5,
                        enter_verify_timeout=2.0, enter_max_retries=3)
    assert enter_attempts[0] == 2, (
        f"expected 2 Enter keypresses (1 lost, 1 succeeded), got {enter_attempts[0]}"
    )
    # First-Enter sleep recorded as pre_enter_settle (0.5)
    assert sleep_calls[0] == 0.5, f"expected pre_enter_settle=0.5 first, got {sleep_calls[:3]}"


def test_send_prompt_first_attempt_succeeds_when_agent_goes_busy_immediately(tmp_path: Path):
    """Common happy path: agent goes busy during or right after the pre-enter
    settle. Only ONE Enter should be sent."""
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    enter_attempts = [0]
    def fake_tmux(*args, **kwargs):
        if "Enter" in args:
            enter_attempts[0] += 1
        class FakeResult:
            returncode = 0; stdout = ""; stderr = ""
        return FakeResult()
    def fake_capture(_session, history=False):
        return "responding Thinking... (esc to cancel, 0m 1s)\n"  # busy from the start
    now_state = [0.0]
    def fake_monotonic():
        now_state[0] += 0.1
        return now_state[0]

    class FakeHandle:
        session = "fake"; cwd = Path("/tmp"); provider = "gemini"; session_id = "sid"

    with (
        patch.object(tmb, "tmux", fake_tmux),
        patch.object(tmb, "capture", fake_capture),
        patch.object(tmb.time, "sleep", lambda _dt: None),
        patch.object(tmb.time, "monotonic", fake_monotonic),
    ):
        tmb.send_prompt(FakeHandle(), "small", enter_max_retries=3,
                        enter_verify_timeout=2.0)
    assert enter_attempts[0] == 1, (
        f"expected 1 Enter on happy path, got {enter_attempts[0]}"
    )


def test_send_prompt_small_prompt_uses_send_keys_not_buffer(tmp_path: Path):
    """Small prompts (< large_threshold) should go via `tmux send-keys -l`,
    not via load-buffer/paste-buffer. Regression guard: the previous
    implementation also had this dispatch; verify it still holds."""
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    called = {"send_keys_l": 0, "buffer": 0, "enter": 0}
    def fake_tmux(*args, **kwargs):
        if "-l" in args and "send-keys" in args:
            called["send_keys_l"] += 1
        if "Enter" in args:
            called["enter"] += 1
        class FakeResult:
            returncode = 0; stdout = ""; stderr = ""
        return FakeResult()
    def fake_buffer(_session, _text):
        called["buffer"] += 1
    def fake_capture(_session, history=False):
        return "responding Thinking...\n"  # busy immediately
    now_state = [0.0]
    def fake_monotonic():
        now_state[0] += 0.1
        return now_state[0]

    class FakeHandle:
        session = "fake"; cwd = Path("/tmp"); provider = "gemini"; session_id = "sid"

    with (
        patch.object(tmb, "tmux", fake_tmux),
        patch.object(tmb, "_send_via_buffer", fake_buffer),
        patch.object(tmb, "capture", fake_capture),
        patch.object(tmb.time, "sleep", lambda _dt: None),
        patch.object(tmb.time, "monotonic", fake_monotonic),
    ):
        tmb.send_prompt(FakeHandle(), "hi", large_threshold=4096)
    assert called["send_keys_l"] == 1
    assert called["buffer"] == 0
    assert called["enter"] == 1


def test_send_prompt_large_prompt_uses_buffer_not_send_keys(tmp_path: Path):
    """Large prompts (>= large_threshold) should go via load-buffer/paste-buffer
    (bracketed-paste mode) — never via send-keys -l which would interpret
    newlines as per-line Enter presses."""
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    called = {"send_keys_l": 0, "buffer": 0, "enter": 0}
    def fake_tmux(*args, **kwargs):
        if "-l" in args and "send-keys" in args:
            called["send_keys_l"] += 1
        if "Enter" in args:
            called["enter"] += 1
        class FakeResult:
            returncode = 0; stdout = ""; stderr = ""
        return FakeResult()
    def fake_buffer(_session, _text):
        called["buffer"] += 1
    def fake_capture(_session, history=False):
        return "responding Thinking...\n"
    now_state = [0.0]
    def fake_monotonic():
        now_state[0] += 0.1
        return now_state[0]

    class FakeHandle:
        session = "fake"; cwd = Path("/tmp"); provider = "gemini"; session_id = "sid"

    huge_prompt = "x" * 8192  # > default 4096
    with (
        patch.object(tmb, "tmux", fake_tmux),
        patch.object(tmb, "_send_via_buffer", fake_buffer),
        patch.object(tmb, "capture", fake_capture),
        patch.object(tmb.time, "sleep", lambda _dt: None),
        patch.object(tmb.time, "monotonic", fake_monotonic),
    ):
        tmb.send_prompt(FakeHandle(), huge_prompt)
    assert called["send_keys_l"] == 0, "large prompt should not use send-keys -l"
    assert called["buffer"] == 1, "large prompt should use paste buffer exactly once"
    assert called["enter"] == 1


def test_send_prompt_small_send_keys_failure_falls_back_to_buffer(tmp_path: Path):
    """If tmux send-keys -l fails (e.g., shell length limit), send_prompt
    must fall back to the buffer path. Regression guard on the existing
    try/except around send-keys -l."""
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    called = {"buffer": 0, "enter": 0}
    def fake_tmux(*args, **kwargs):
        # Fail send-keys -l, succeed on Enter
        if "-l" in args and "send-keys" in args:
            raise Exception("simulated send-keys failure")
        if "Enter" in args:
            called["enter"] += 1
        class FakeResult:
            returncode = 0; stdout = ""; stderr = ""
        return FakeResult()
    def fake_buffer(_session, _text):
        called["buffer"] += 1
    def fake_capture(_session, history=False):
        return "responding Thinking...\n"
    now_state = [0.0]
    def fake_monotonic():
        now_state[0] += 0.1
        return now_state[0]

    class FakeHandle:
        session = "fake"; cwd = Path("/tmp"); provider = "gemini"; session_id = "sid"

    with (
        patch.object(tmb, "tmux", fake_tmux),
        patch.object(tmb, "_send_via_buffer", fake_buffer),
        patch.object(tmb, "capture", fake_capture),
        patch.object(tmb.time, "sleep", lambda _dt: None),
        patch.object(tmb.time, "monotonic", fake_monotonic),
    ):
        tmb.send_prompt(FakeHandle(), "small", large_threshold=4096)
    assert called["buffer"] == 1, "send-keys failure must trigger buffer fallback"
    assert called["enter"] == 1


def test_send_prompt_claude_provider_uses_claude_busy_markers(tmp_path: Path):
    """When the handle's provider is claude, the verify-busy check must
    recognize claude's busy markers, not just gemini's."""
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    enter_attempts = [0]
    def fake_tmux(*args, **kwargs):
        if "Enter" in args:
            enter_attempts[0] += 1
        class FakeResult:
            returncode = 0; stdout = ""; stderr = ""
        return FakeResult()
    def fake_capture(_session, history=False):
        return "thinking... (esc to interrupt) \n"  # claude-style busy
    now_state = [0.0]
    def fake_monotonic():
        now_state[0] += 0.1
        return now_state[0]

    class FakeHandle:
        session = "fake"; cwd = Path("/tmp"); provider = "claude"; session_id = "sid"

    with (
        patch.object(tmb, "tmux", fake_tmux),
        patch.object(tmb, "capture", fake_capture),
        patch.object(tmb.time, "sleep", lambda _dt: None),
        patch.object(tmb.time, "monotonic", fake_monotonic),
    ):
        tmb.send_prompt(FakeHandle(), "hi")
    assert enter_attempts[0] == 1, (
        f"claude busy marker should satisfy verify, got {enter_attempts[0]} Enters"
    )


def test_send_prompt_stops_resending_after_max_retries(tmp_path: Path):
    """If the agent never goes busy (e.g., gemini hung), send_prompt must
    return after enter_max_retries — never raise, never block forever."""
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    enter_attempts = [0]
    def fake_tmux(*args, **kwargs):
        if "Enter" in args:
            enter_attempts[0] += 1
        class FakeResult:
            returncode = 0
            stdout = ""
            stderr = ""
        return FakeResult()
    def fake_capture(_session, history=False):
        return "YOLO mode  Type your message or @path/to/file\n"  # never busy
    now_state = [0.0]
    def fake_monotonic():
        now_state[0] += 0.1
        return now_state[0]

    class FakeHandle:
        session = "fake-session"
        cwd = Path("/tmp")
        provider = "gemini"
        session_id = "fake-sid"

    with (
        patch.object(tmb, "tmux", fake_tmux),
        patch.object(tmb, "capture", fake_capture),
        patch.object(tmb.time, "sleep", lambda _dt: None),
        patch.object(tmb.time, "monotonic", fake_monotonic),
    ):
        tmb.send_prompt(FakeHandle(), "prompt", pre_enter_settle=0.5,
                        enter_verify_timeout=1.0, enter_max_retries=3)
    assert enter_attempts[0] == 3, (
        f"expected exactly enter_max_retries=3 attempts, got {enter_attempts[0]}"
    )


def test_wait_until_idle_session_transcript_growth_keeps_agent_alive(tmp_path: Path):
    """liveness_probe returning increasing mtimes — simulating a streaming
    session transcript file being appended to as the agent receives
    tokens — MUST prevent apparent_stall from firing, no matter how
    constant the pane / how long the run."""
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    now_state = [0.0]
    def fake_monotonic():
        return now_state[0]
    def fake_monotonic_ns():
        return int(now_state[0] * 1_000_000_000)
    def fake_sleep(dt):
        now_state[0] += dt

    # Frozen pane — constant text + constant busy marker.
    frozen_screen = (
        "workspace (/directory)\n"
        "  master sandbox\n"
        "  gemini-3.1-pro-preview\n"
        "responding Thinking... (esc to cancel, 0m 3s) ? for shortcuts\n"
    )

    done_file = tmp_path / "done.marker"
    probe_calls = [0]
    def probe():
        probe_calls[0] += 1
        # After running well past apparent_stall window, write done_file
        # so the loop exits cleanly.
        if probe_calls[0] == 200:
            done_file.write_text("", encoding="utf-8")
        return probe_calls[0]   # strictly growing — liveness signal fires

    class FakeHandle:
        session = "fake-session"
        cwd = Path("/tmp")
        provider = "gemini"
        session_id = "fake-sid"

    with (
        patch.object(tmb, "capture", lambda _s, history=True: frozen_screen),
        patch.object(tmb, "pane_dead", lambda _session: False),
        patch.object(tmb.time, "monotonic", fake_monotonic),
        patch.object(tmb.time, "monotonic_ns", fake_monotonic_ns),
        patch.object(tmb.time, "sleep", fake_sleep),
    ):
        done, reason = tmb.wait_until_idle(
            FakeHandle(),
            min_stable_seconds=60.0,
            poll_interval=1.0,
            total_timeout=3600.0,
            done_file=done_file,
            require_change_first=False,
            workspace_paths=None,
            liveness_probe=probe,
            apparent_stall_seconds=60.0,    # would've tripped 3x without probe
        )
    assert reason == "done_file", (
        f"expected clean exit via done_file, got reason={reason} after "
        f"{probe_calls[0]} polls (apparent_stall misfired despite transcript growth)"
    )


def test_wait_until_idle_workspace_fs_writes_keep_agent_alive(tmp_path: Path):
    """Tool-call FS writes in workspace_paths must reset the liveness clock.
    We simulate a busy agent with continuous FS activity for well past
    apparent_stall_seconds, then let a done_file appear to exit the loop.
    apparent_stall must NOT fire during the FS-active period.
    """
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    now_state = [0.0]
    def fake_monotonic():
        return now_state[0]
    def fake_monotonic_ns():
        return int(now_state[0] * 1_000_000_000)
    def fake_sleep(dt):
        now_state[0] += dt

    frozen_screen = (
        "workspace (/directory)\n"
        "  claude-opus-4-6\n"
        "responding Thinking... esc to interrupt\n"
    )

    ws = tmp_path / "ws"
    ws.mkdir()
    done_file = tmp_path / "done.marker"

    # Continuous FS activity — bumps every poll.
    mtime_counter = [1_000_000_000_000]
    poll_count = [0]
    def fake_mtime(paths):
        poll_count[0] += 1
        mtime_counter[0] += 500_000_000
        # After 200 simulated seconds (well past apparent_stall_seconds=60),
        # write the done_file so the loop exits cleanly via the done path.
        if poll_count[0] == 200:
            done_file.write_text("", encoding="utf-8")
        return mtime_counter[0]

    class FakeHandle:
        session = "fake-session"
        cwd = Path("/tmp")
        provider = "claude"
        session_id = "fake-sid"

    with (
        patch.object(tmb, "capture", lambda _s, history=True: frozen_screen),
        patch.object(tmb, "pane_dead", lambda _session: False),
        patch.object(tmb, "_workspace_latest_mtime_ns", fake_mtime),
        patch.object(tmb.time, "monotonic", fake_monotonic),
        patch.object(tmb.time, "monotonic_ns", fake_monotonic_ns),
        patch.object(tmb.time, "sleep", fake_sleep),
    ):
        done, reason = tmb.wait_until_idle(
            FakeHandle(),
            min_stable_seconds=60.0,
            poll_interval=1.0,
            total_timeout=3600.0,
            done_file=done_file,
            require_change_first=False,
            workspace_paths=[ws],
            liveness_probe=None,
            apparent_stall_seconds=60.0,    # would've tripped 3x without FS
        )
    # The loop should exit via done_file, never via apparent_stall.
    assert reason == "done_file", (
        f"expected clean exit via done_file, got reason={reason} after "
        f"{poll_count[0]} polls (apparent_stall misfired on live FS writes)"
    )


def test_wait_until_idle_fires_when_all_positive_signals_are_stale():
    """No workspace FS writes AND session-transcript mtime is flat —
    apparent_stall MUST fire. This is a real wedge (reasoning thread dead
    even if the TUI is still rendering)."""
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    now_state = [0.0]
    def fake_monotonic():
        return now_state[0]
    def fake_monotonic_ns():
        return int(now_state[0] * 1_000_000_000)
    def fake_sleep(dt):
        now_state[0] += dt

    # Even include a ticking timer — this should NOT save us. TUI tick is
    # not a positive work signal.
    def fake_capture(_s, history=True):
        secs = int(now_state[0])
        footer = (
            f"responding Thinking... (esc to cancel, {secs // 60}m {secs % 60}s)"
            " ? for shortcuts\n"
        )
        return f"workspace (/directory)\n  master sandbox\n{footer}"

    # Session-transcript probe frozen at constant mtime (no file growth).
    def probe():
        return 42

    class FakeHandle:
        session = "fake-session"
        cwd = Path("/tmp")
        provider = "gemini"
        session_id = "fake-sid"

    with (
        patch.object(tmb, "capture", fake_capture),
        patch.object(tmb, "pane_dead", lambda _session: False),
        patch.object(tmb.time, "monotonic", fake_monotonic),
        patch.object(tmb.time, "monotonic_ns", fake_monotonic_ns),
        patch.object(tmb.time, "sleep", fake_sleep),
    ):
        done, reason = tmb.wait_until_idle(
            FakeHandle(),
            min_stable_seconds=60.0,
            poll_interval=1.0,
            total_timeout=3600.0,
            done_file=None,
            require_change_first=False,
            workspace_paths=None,
            liveness_probe=probe,
            apparent_stall_seconds=60.0,
        )
    assert not done
    assert reason.startswith("apparent_stall_"), (
        f"apparent_stall failed to fire despite all positive signals stale: reason={reason}"
    )


# ---- terminal-API-error detector -------------------------------------------

def test_agent_has_terminal_api_error_detects_gemini_bracketed_marker():
    screen = (
        "workspace (/home/sandbox-user/math/example-run)\n"
        "[API Error: Server returned 500 (Status: 500)]\n"
        "YOLO Ctrl+Y   Type your message or @path/to/file\n"
    )
    assert agent_has_terminal_api_error(screen, "gemini") == "[API Error:"
    # claude path: same text shouldn't fire — marker is gemini-specific.
    assert agent_has_terminal_api_error(screen, "claude") is None
    # No-error pane: None.
    assert agent_has_terminal_api_error(
        "workspace (/dir)\n  master sandbox\n  gemini-3.1-pro-preview\n",
        "gemini",
    ) is None


def test_wait_until_idle_fires_on_persistent_api_error_at_idle():
    """gemini pane shows `[API Error: ...]` continuously while NOT busy.
    The new fast-path detector must fire after api_error_idle_seconds and
    return (False, api_error_idle_*). Without it, the burst would wait the
    full stable_after_busy window (5400 s in prod) before exiting."""
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    now_state = [0.0]
    def fake_monotonic():
        return now_state[0]
    def fake_monotonic_ns():
        return int(now_state[0] * 1_000_000_000)
    def fake_sleep(dt):
        now_state[0] += dt

    # Pane: error marker persistent, NO busy marker, prompt line idle.
    idle_error_screen = (
        "workspace (/home/sandbox-user/math/example-run)\n"
        "[API Error: Internal Server Error (Status: 500)]\n"
        "YOLO Ctrl+Y   Type your message or @path/to/file\n"
    )

    class FakeHandle:
        session = "fake-session"
        cwd = Path("/tmp")
        provider = "gemini"
        session_id = "fake-sid"

    with (
        patch.object(tmb, "capture", lambda _s, history=True: idle_error_screen),
        patch.object(tmb, "pane_dead", lambda _session: False),
        patch.object(tmb, "maybe_auto_confirm_gemini_prompt",
                     lambda _h, _screen: None),
        patch.object(tmb.time, "monotonic", fake_monotonic),
        patch.object(tmb.time, "monotonic_ns", fake_monotonic_ns),
        patch.object(tmb.time, "sleep", fake_sleep),
    ):
        done, reason = tmb.wait_until_idle(
            FakeHandle(),
            min_stable_seconds=5400.0,    # prod stable threshold
            poll_interval=1.0,
            total_timeout=3600.0,
            done_file=None,
            require_change_first=False,
            workspace_paths=None,
            liveness_probe=lambda: 42,
            apparent_stall_seconds=0.0,
            api_error_idle_seconds=60.0,
        )
    assert not done, f"expected failure, got {(done, reason)}"
    assert reason.startswith("api_error_idle_"), (
        f"expected api_error_idle_*, got {reason}"
    )
    # Should fire near the threshold (60s), well before any other detector.
    assert now_state[0] < 120.0, (
        f"api-error fast path took {now_state[0]:.1f}s, expected ~60s"
    )


def test_wait_until_idle_does_not_fire_api_error_while_busy_retrying():
    """gemini-cli flashes `[API Error:` between its own internal retries
    while the CLI is still busy. The new detector must NOT fire in that
    case — only when the marker persists AND busy markers are absent."""
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    now_state = [0.0]
    def fake_monotonic():
        return now_state[0]
    def fake_monotonic_ns():
        return int(now_state[0] * 1_000_000_000)
    def fake_sleep(dt):
        now_state[0] += dt

    # Error marker visible AND busy marker visible — gemini is retrying.
    # done_file appears after 200 simulated seconds (way past the 60s
    # api_error_idle window), so the loop must exit via done_file, never
    # via api_error_idle_*.
    done_file_path = Path(tempfile.mkdtemp()) / "done.marker"
    poll_count = [0]
    def fake_capture(_s, history=True):
        poll_count[0] += 1
        if now_state[0] > 200.0:
            done_file_path.write_text("done")
        return (
            "workspace (/home/sandbox-user/math/example-run)\n"
            "[API Error: 503 (transient)]\n"
            "responding Thinking... (esc to cancel, 0m 30s) ? for shortcuts\n"
        )

    class FakeHandle:
        session = "fake-session"
        cwd = Path("/tmp")
        provider = "gemini"
        session_id = "fake-sid"

    with (
        patch.object(tmb, "capture", fake_capture),
        patch.object(tmb, "pane_dead", lambda _session: False),
        patch.object(tmb, "maybe_auto_confirm_gemini_prompt",
                     lambda _h, _screen: None),
        patch.object(tmb.time, "monotonic", fake_monotonic),
        patch.object(tmb.time, "monotonic_ns", fake_monotonic_ns),
        patch.object(tmb.time, "sleep", fake_sleep),
    ):
        done, reason = tmb.wait_until_idle(
            FakeHandle(),
            min_stable_seconds=60.0,
            poll_interval=1.0,
            total_timeout=3600.0,
            done_file=done_file_path,
            require_change_first=False,
            workspace_paths=None,
            liveness_probe=lambda: int(now_state[0]) + 1,  # growing
            apparent_stall_seconds=0.0,
            api_error_idle_seconds=60.0,
        )
    assert done, f"expected done_file, got {(done, reason)}"
    assert reason == "done_file", f"expected done_file, got {reason}"


def test_wait_until_idle_resets_api_error_when_marker_flickers_off():
    """gemini-cli may briefly clear the `[API Error:` line during a retry
    (e.g. pane redraw) even while busy markers are gone. The detector must
    re-arm cleanly: if the marker disappears for one poll then reappears,
    the persistence clock must start over, not act on accumulated time."""
    from unittest.mock import patch
    import trellis.agents.tmux_backend as tmb

    now_state = [0.0]
    def fake_monotonic():
        return now_state[0]
    def fake_monotonic_ns():
        return int(now_state[0] * 1_000_000_000)
    def fake_sleep(dt):
        now_state[0] += dt

    # First 40s: error visible idle. Then 1 poll without it. Then back to
    # error visible. With api_error_idle_seconds=60, the detector should
    # NOT fire at t=40 (only 40s seen), should reset at t=41 (marker
    # gone), then fire ~60s after the second window starts.
    idle_with_error = (
        "workspace (/dir)\n"
        "[API Error: 500]\n"
        "YOLO Ctrl+Y   Type your message or @path/to/file\n"
    )
    idle_without_error = (
        "workspace (/dir)\n"
        "  (the agent is thinking through next steps)\n"  # NOT a busy marker
        "YOLO Ctrl+Y   Type your message or @path/to/file\n"
    )
    def fake_capture(_s, history=True):
        if 40.5 < now_state[0] < 41.5:
            return idle_without_error
        return idle_with_error

    class FakeHandle:
        session = "fake-session"
        cwd = Path("/tmp")
        provider = "gemini"
        session_id = "fake-sid"

    with (
        patch.object(tmb, "capture", fake_capture),
        patch.object(tmb, "pane_dead", lambda _session: False),
        patch.object(tmb, "maybe_auto_confirm_gemini_prompt",
                     lambda _h, _screen: None),
        patch.object(tmb.time, "monotonic", fake_monotonic),
        patch.object(tmb.time, "monotonic_ns", fake_monotonic_ns),
        patch.object(tmb.time, "sleep", fake_sleep),
    ):
        done, reason = tmb.wait_until_idle(
            FakeHandle(),
            min_stable_seconds=5400.0,
            poll_interval=1.0,
            total_timeout=3600.0,
            done_file=None,
            require_change_first=False,
            workspace_paths=None,
            liveness_probe=lambda: 42,  # frozen
            apparent_stall_seconds=0.0,
            api_error_idle_seconds=60.0,
        )
    assert not done
    assert reason.startswith("api_error_idle_"), (
        f"expected api_error_idle_*, got {reason}"
    )
    # Must have re-armed after the flicker: total time ~= 41 + 60 = 101s,
    # NOT 60s. If the timer hadn't reset on the flicker, the fail would
    # have fired at t=60s.
    assert now_state[0] > 95.0, (
        f"api-error detector failed to re-arm after flicker (fired at "
        f"{now_state[0]:.1f}s, expected >95s)"
    )


# -------------- main --------------

def main() -> int:
    tests = [v for k, v in globals().items() if k.startswith("test_") and callable(v)]
    for t in tests:
        run(t.__name__, t)
    passed = sum(1 for _, ok, _ in _RESULTS if ok)
    failed = [(n, e) for n, ok, e in _RESULTS if not ok]
    print(json.dumps({
        "passed": passed,
        "failed": len(failed),
        "failures": [{"test": n, "error": e} for n, e in failed],
    }, indent=2))
    return 0 if not failed else 2


if __name__ == "__main__":
    sys.exit(main())
