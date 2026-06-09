"""Burst dispatch: routes to the right agent backend.

The supervisor calls run_worker_burst() and run_reviewer_burst().
These dispatch to:
- codex_headless: for Codex (proven reliable headless mode)
- tmux_backend: for Claude and Gemini (tmux-driven interactive driver)
- script_headless: for fallback providers that do not have a dedicated backend

All backends return the same BurstResult type.
"""

from __future__ import annotations

import json
import re
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

from trellis.adapters import BurstResult, ProviderConfig
from trellis.config import SandboxConfig


# ---------------------------------------------------------------------------
# Rate limit detection (shared across backends)
# ---------------------------------------------------------------------------

RATE_LIMIT_PATTERNS = [
    "rate limit", "rate_limit", "ratelimit", "too many requests", "429",
    "resource_exhausted", "model_capacity_exhausted", "quota exceeded",
    "usage limit", "credit balance is too low", "overloaded_error",
    "hit your limit", "exceeded retry limit",
]

FAST_RETRYABLE_ERROR_PATTERNS = [
    "agent died immediately after receiving prompt",
]


def _maybe_prepare_gemini_auth(
    config: ProviderConfig,
    *,
    burst_home: Optional[Path] = None,
) -> None:
    if config.provider != "gemini":
        return
    from trellis.gemini_accounts import maybe_ensure_budget

    maybe_ensure_budget(burst_home=burst_home)


def is_rate_limited(output: str) -> bool:
    lowered = output.lower()
    return any(p in lowered for p in RATE_LIMIT_PATTERNS)


def is_fast_retryable_error(output: str) -> bool:
    lowered = output.lower()
    return any(p in lowered for p in FAST_RETRYABLE_ERROR_PATTERNS)


_EXHAUSTED_MODEL_RE = re.compile(
    r'No capacity available for model (\S+)',
    re.IGNORECASE,
)
_EXHAUSTED_MODEL_JSON_RE = re.compile(
    r'"model":\s*"([^"]+)"',
)


def extract_exhausted_model(text: str) -> Optional[str]:
    """Parse the model name from a MODEL_CAPACITY_EXHAUSTED error.

    Returns the model name (e.g., 'gemini-3-flash-preview') or None.
    """
    m = _EXHAUSTED_MODEL_RE.search(text)
    if m:
        return m.group(1)
    if "model_capacity_exhausted" in text.lower():
        m = _EXHAUSTED_MODEL_JSON_RE.search(text)
        if m:
            return m.group(1)
    return None


# ---------------------------------------------------------------------------
# Retry wrapper (shared across backends)
# ---------------------------------------------------------------------------

def run_with_retry(
    fn,
    *,
    max_retries: int = 5,
    max_post_agent_retries: int = 1,
    base_delay: float = 60.0,
    max_delay: float = 900.0,
    rate_limit_delay: float = 120.0,
    config: Optional[ProviderConfig] = None,
    port: Optional[int] = None,
) -> BurstResult:
    """Retry a burst function with exponential backoff on rate limits.

    Bug X principled fix: the retry budget is split.

    * `max_retries` governs PRE-agent retries: rate-limit detection, model
      fallback, gemini startup failures (429 before the backend captures
      output). The agent never produced disk writes for these; retrying is
      safe and burns no kernel-visible budget.

    * `max_post_agent_retries` (default 1) governs POST-agent retries: any
      non-rate-limit failure where the agent may have already started
      mutating disk (`is_fast_retryable_error`, generic `Burst failed`).
      Keep this small — the kernel now owns the retry decision via
      `RetryOutcomeKind::Transport`, so silent loops here just hide
      transport failures from the kernel and corrupt the
      `before_snapshot` baseline (Bug X cycle-49 root cause).

    When config has fallback_models and the error identifies a specific
    exhausted model, attempts to switch to the next available fallback
    via /model command (no server restart) before retrying.
    """
    from trellis.model_availability import get_availability
    availability = get_availability()

    last_result = None
    # Track pre-agent vs post-agent attempts separately so we can apply the
    # split budget without interleaving messing up the count.
    pre_agent_attempt = 0
    post_agent_attempt = 0
    # Total iteration cap: pre-agent budget + post-agent budget + 1 for the
    # initial try. Don't loop forever even if one side keeps yielding the
    # other's failure mode.
    max_total = max_retries + max_post_agent_retries + 1
    for _ in range(max_total):
        result = fn()
        last_result = result
        if result.ok:
            return result

        combined_output = result.captured_output + " " + result.error
        rate_limited = is_rate_limited(combined_output)

        # Gemini startup failures (Failed to send message) are often 429s
        # that happen before the backend can capture the error text.
        # Treat them as rate-limited even without explicit fallback models so
        # the outer retry policy still gets a chance to recover.
        gemini_startup_fail = (
            not rate_limited
            and config
            and config.provider == "gemini"
            and "Failed to send message" in result.error
        )
        if gemini_startup_fail:
            rate_limited = True
            # Use current model as the exhausted one
            if config.model:
                combined_output += f" No capacity available for model {config.model}"

        if rate_limited:
            # Try model fallback before sleeping
            exhausted = extract_exhausted_model(combined_output)
            if exhausted and config and config.fallback_models:
                availability.mark_unavailable(exhausted, f"429 capacity exhausted")
                fallback = availability.pick_available(config.fallback_models)
                if fallback:
                    print(f"  Model {exhausted} exhausted, falling back to {fallback}")
                    config.model = fallback
                    # The active backend (tmux_backend for claude/gemini) handles
                    # the in-session switch itself — `run_gemini_burst` pops from
                    # its `active_fallbacks` list on a MODEL_CAPACITY_EXHAUSTED
                    # event and continues with the new model. We only need to
                    # update `config.model` here so the next outer attempt sees
                    # the right value.
                    continue  # retry immediately with new model, no backoff

            if pre_agent_attempt >= max_retries:
                result.error = f"Rate limited after {max_retries} retries: {result.error}"
                blocked = availability.status()
                if blocked:
                    result.error += f" Blocked models: {blocked}"
                return result
            delay = min(rate_limit_delay * (2 ** pre_agent_attempt), max_delay)
            print(f"  Rate limited (attempt {pre_agent_attempt + 1}/{max_retries}), waiting {delay:.0f}s...")
            time.sleep(delay)
            pre_agent_attempt += 1
            continue

        if is_fast_retryable_error(combined_output):
            # Post-agent failure: agent may have written to disk before
            # crashing. Bug X principled fix: cap retries at
            # max_post_agent_retries (default 1) and let the kernel decide
            # whether to retry via RetryOutcomeKind::Transport.
            if post_agent_attempt >= max_post_agent_retries:
                return result
            delay = min(5.0, max_delay)
            print(f"  Burst crashed before completion (post-agent attempt {post_agent_attempt + 1}/{max_post_agent_retries}), waiting {delay:.0f}s...")
            time.sleep(delay)
            post_agent_attempt += 1
            continue

        # Generic post-agent failure. Same cap as fast-retryable.
        if post_agent_attempt >= max_post_agent_retries:
            return result
        delay = min(base_delay * (2 ** post_agent_attempt), max_delay)
        print(f"  Burst failed (post-agent attempt {post_agent_attempt + 1}/{max_post_agent_retries}), waiting {delay:.0f}s...")
        time.sleep(delay)
        post_agent_attempt += 1
    return last_result or BurstResult(ok=False, exit_code=None, captured_output="",
                                       duration_seconds=0, error="No attempts made")


# ---------------------------------------------------------------------------
# Dispatch functions
# ---------------------------------------------------------------------------

def run_worker_burst(
    config: ProviderConfig,
    prompt: str,
    *,
    session_name: str,
    work_dir: Path,
    timeout_seconds: float = 7200.0,
    startup_timeout_seconds: float = 3600.0,
    max_rate_limit_retries: int = 5,
    log_dir: Optional[Path] = None,
    port: Optional[int] = None,
    session_scope: str = "",
    fresh: bool = False,
    done_file: Optional[Path] = None,
    artifact_prefix: Optional[str] = None,
    sandbox: Optional[SandboxConfig] = None,
    burst_home: Optional[Path] = None,
    **_kwargs,
) -> BurstResult:
    """Run a worker burst -- dispatches to the right backend."""
    handoff_file = done_file or work_dir / "worker_handoff.json"
    handoff_file.unlink(missing_ok=True)
    prefix = artifact_prefix or handoff_file.stem or "worker"

    def _run():
        _maybe_prepare_gemini_auth(
            config,
            burst_home=burst_home,
        )
        if config.provider == "codex":
            from trellis.agents.codex_headless import run
            return run(config, prompt, role="worker", session_name=session_name,
                      work_dir=work_dir,
                      startup_timeout=startup_timeout_seconds,
                      burst_timeout=timeout_seconds, log_dir=log_dir,
                      session_scope=session_scope,
                      artifact_prefix=prefix, fresh=fresh,
                      done_file=handoff_file,
                      sandbox=sandbox, burst_home=burst_home)

        if config.provider in {"claude", "gemini"}:
            from trellis.agents.tmux_backend import run
            return run(config, prompt, role="worker", session_name=session_name,
                      work_dir=work_dir,
                      timeout=timeout_seconds,
                      startup_timeout=startup_timeout_seconds,
                      port=port, session_scope=session_scope, fresh=fresh,
                      log_dir=log_dir,
                      artifact_prefix=prefix,
                      done_file=handoff_file,
                      sandbox=sandbox,
                      burst_home=burst_home)

        # Unknown providers: script-based headless (-p mode)
        from trellis.agents.script_headless import run
        return run(config, prompt, role="worker", session_name=session_name,
                  work_dir=work_dir,
                  startup_timeout=startup_timeout_seconds,
                  burst_timeout=timeout_seconds, log_dir=log_dir,
                  session_scope=session_scope,
                  artifact_prefix=prefix,
                  done_file=handoff_file,
                  sandbox=sandbox,
                  burst_home=burst_home)

    # Audit followup #2 (Problem C): force `max_post_agent_retries=0` for
    # worker bursts. A post-agent failure is exactly the case where the
    # agent may have already mutated `Tablet/`; an in-process retry runs
    # the second attempt against the now-dirty disk before the kernel
    # ever sees a transport-failure signal, and that second attempt
    # absorbs the unauthorized writes into its `before_snapshot`. Let
    # the kernel own the retry decision via `RetryOutcomeKind::Transport`
    # — it knows how to restore `active_worker_base` between attempts.
    return run_with_retry(_run, max_retries=max_rate_limit_retries, rate_limit_delay=120.0,
                          max_post_agent_retries=0,
                          config=config, port=port)


def run_reviewer_burst(
    config: ProviderConfig,
    prompt: str,
    *,
    session_name: str,
    work_dir: Path,
    role: str = "reviewer",
    timeout_seconds: float = 3600.0,
    startup_timeout_seconds: float = 3600.0,
    max_rate_limit_retries: int = 3,
    log_dir: Optional[Path] = None,
    port: Optional[int] = None,
    session_scope: str = "",
    fresh: bool = False,
    done_file: Optional[Path] = None,
    artifact_prefix: Optional[str] = None,
    sandbox: Optional[SandboxConfig] = None,
    burst_home: Optional[Path] = None,
    **_kwargs,
) -> BurstResult:
    """Run a reviewer burst -- dispatches to the right backend.

    If fresh=True, starts a new agent session (no context from prior cycles).
    Used for stateless verification agents.
    """

    def _run():
        _maybe_prepare_gemini_auth(
            config,
            burst_home=burst_home,
        )
        prefix = artifact_prefix or (done_file.stem if done_file is not None else "reviewer")
        if config.provider == "codex":
            from trellis.agents.codex_headless import run
            return run(config, prompt, role=role, session_name=session_name,
                      work_dir=work_dir,
                      startup_timeout=startup_timeout_seconds,
                      burst_timeout=timeout_seconds, log_dir=log_dir,
                      session_scope=session_scope,
                      artifact_prefix=prefix, fresh=fresh,
                      done_file=done_file or work_dir / "reviewer_decision.json",
                      sandbox=sandbox, burst_home=burst_home)

        if config.provider in {"claude", "gemini"}:
            from trellis.agents.tmux_backend import run
            return run(config, prompt, role=role, session_name=session_name,
                      work_dir=work_dir,
                      timeout=timeout_seconds,
                      startup_timeout=startup_timeout_seconds,
                      port=port, session_scope=session_scope, fresh=fresh,
                      log_dir=log_dir,
                      artifact_prefix=prefix,
                      done_file=done_file or work_dir / "reviewer_decision.json",
                      sandbox=sandbox,
                      burst_home=burst_home)

        # Unknown providers: script-based headless
        from trellis.agents.script_headless import run
        return run(config, prompt, role=role, session_name=session_name,
                  work_dir=work_dir,
                  startup_timeout=startup_timeout_seconds,
                  burst_timeout=timeout_seconds, log_dir=log_dir,
                  session_scope=session_scope,
                  artifact_prefix=prefix,
                  done_file=done_file or work_dir / "reviewer_decision.json",
                  sandbox=sandbox,
                  burst_home=burst_home)

    return run_with_retry(_run, max_retries=max_rate_limit_retries, rate_limit_delay=60.0,
                          config=config, port=port)


# ---------------------------------------------------------------------------
# JSON extraction (shared utility)
# ---------------------------------------------------------------------------

def _clean_terminal_json(text: str) -> str:
    """Clean terminal-formatted text for JSON parsing.

    Agent output from a tmux-driven CLI may have trailing whitespace
    padding on each line (terminal width) and line-wrapping inside
    string values. Strip trailing spaces and rejoin continuation lines.
    """
    # Strip ✦ prefix (Gemini response marker)
    text = text.strip()
    if text.startswith("✦"):
        text = text[1:].strip()
    # Strip trailing whitespace from each line, rejoin
    lines = [line.rstrip() for line in text.split("\n")]
    # Collapse lines that are continuations inside JSON strings:
    # A line that starts with spaces and doesn't start a new JSON key
    # is likely a wrapped continuation of the previous line.
    collapsed = []
    for line in lines:
        stripped = line.lstrip()
        if collapsed and stripped and not stripped.startswith(("{", "}", "[", "]", '"')):
            # Continuation line -- append to previous
            collapsed[-1] = collapsed[-1] + " " + stripped
        else:
            collapsed.append(line)
    return "\n".join(collapsed)


def extract_json_decision(text: str) -> Optional[Dict[str, Any]]:
    """Extract a JSON decision from agent output."""
    text = _clean_terminal_json(text)
    try:
        wrapper = json.loads(text)
        if isinstance(wrapper, dict):
            if "decision" in wrapper:
                return wrapper
            result_text = str(wrapper.get("result", ""))
            return extract_json_decision(result_text)
    except json.JSONDecodeError:
        pass

    code_block = re.search(r"```(?:json)?\s*\n?(.*?)```", text, re.DOTALL)
    if code_block:
        try:
            parsed = json.loads(code_block.group(1).strip())
            if isinstance(parsed, dict):
                return parsed
        except json.JSONDecodeError:
            pass

    best = None
    depth = 0
    start_idx = -1
    for i, ch in enumerate(text):
        if ch == '{':
            if depth == 0:
                start_idx = i
            depth += 1
        elif ch == '}':
            depth -= 1
            if depth == 0 and start_idx >= 0:
                try:
                    parsed = json.loads(text[start_idx:i + 1])
                    if isinstance(parsed, dict) and "decision" in parsed:
                        best = parsed
                except json.JSONDecodeError:
                    pass
                start_idx = -1
    return best


# ---------------------------------------------------------------------------
# tmux helpers (used by codex_headless backend)
# ---------------------------------------------------------------------------

def tmux_cmd(*args: str, check: bool = False, timeout: int = 10):
    import subprocess
    from trellis.tmux_socket import tmux_argv
    return subprocess.run(
        tmux_argv(*args), capture_output=True, text=True, timeout=timeout, check=check,
    )

def tmux_has_session(session: str) -> bool:
    return tmux_cmd("has-session", "-t", session).returncode == 0

def tmux_ensure_session(session: str) -> None:
    if not tmux_has_session(session):
        tmux_cmd("new-session", "-d", "-s", session, "-x", "220", "-y", "50")

def tmux_kill_window(session: str, window: str) -> None:
    tmux_cmd("kill-window", "-t", f"{session}:{window}")

def tmux_pane_is_dead(pane_id: str) -> bool:
    result = tmux_cmd("display-message", "-p", "-t", pane_id, "#{pane_dead}", check=False)
    if result.returncode != 0:
        return True
    return result.stdout.strip() == "1"
