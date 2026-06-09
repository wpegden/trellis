"""Codex headless backend: script-based `codex exec`.

This is the proven-reliable approach for Codex. The bash script wraps
the codex command, reads the prompt from a file, and writes start/exit
marker files via trap EXIT.

When a validated artifact `done_file` is configured, that marker is also
treated as authoritative completion for wrapper-style requests after a short
grace period, so the bridge does not remain blocked on a Codex subprocess that
has already finished the checked artifact.
"""

from __future__ import annotations

import json
import os
import re
import shlex
import subprocess
import time
from pathlib import Path
from typing import Optional

from trellis.adapters import BurstResult, ProviderConfig
from trellis.chat_history import ensure_chat_file_link
from trellis.config import SandboxConfig
from trellis.gemini_accounts import gemini_api_env_keys_to_forward
from trellis.host_runtime import DEFAULT_WORKER_PATH, worker_elan_home, worker_path_env
from trellis.sandbox import wrap_command

WORKER_PATH = DEFAULT_WORKER_PATH
DONE_FILE_EXIT_GRACE_SECONDS = 2.0
# After done_file appears, give codex a longer window to finish its turn
# and emit `{"type":"turn.completed","usage":{...}}` to output.log. Without
# this we kill codex right after the done_file write, before it streams
# the closing turn — which loses usage/cost reporting in the cost ledger.
# Polled with early-exit on turn.completed; this is just the cap.
DONE_FILE_TURN_COMPLETED_WAIT_SECONDS = 60.0


def _artifact_prefix(prefix: Optional[str], role: str) -> str:
    base = prefix or role
    base = re.sub(r"[^A-Za-z0-9_.-]+", "_", base).strip("._-") or role
    return base[:80]


def _session_scope_key(
    role: str,
    session_scope: str,
    config: Optional[ProviderConfig] = None,
) -> str:
    scope = re.sub(r"[^A-Za-z0-9_.-]+", "_", str(session_scope or "")).strip("._-")
    provider = re.sub(
        r"[^A-Za-z0-9_.-]+",
        "_",
        str(getattr(config, "provider", "") or ""),
    ).strip("._-")
    model = re.sub(
        r"[^A-Za-z0-9_.-]+",
        "_",
        str(getattr(config, "model", "") or "auto"),
    ).strip("._-")
    effort = re.sub(
        r"[^A-Za-z0-9_.-]+",
        "_",
        str(getattr(config, "effort", "") or ""),
    ).strip("._-")
    parts = [role]
    if scope:
        parts.append(scope)
    if provider:
        parts.append(provider)
    if model:
        parts.append(model)
    if effort:
        parts.append(effort)
    return "-".join(parts)


def _session_state_path(
    work_dir: Path,
    role: str,
    session_scope: str,
    config: Optional[ProviderConfig] = None,
) -> Path:
    session_dir = work_dir / ".trellis" / "sessions"
    session_dir.mkdir(parents=True, exist_ok=True)
    return session_dir / f"codex-{_session_scope_key(role, session_scope, config)}.json"


FORCE_FRESH_NEXT_SENTINEL = Path("/tmp/trellis-force-fresh-next")
"""One-shot operator escape hatch: if this file exists when the next
worker/reviewer burst is about to launch, ``_load_persisted_thread_id``
returns ``""`` (so the launcher omits the ``codex exec resume <thread>``
arguments and codex starts a clean session), and the sentinel file is
deleted so subsequent bursts revert to normal resume behaviour.

Use case: the operator changed prompts / tooling guidance in a way that
should NOT be conditioned on the prior burst's transcript, and the
canonical kernel-driven fresh path (reviewer's
``next_worker_context_mode: "fresh"`` or first-time ``(kind, phase)``
issuance) doesn't apply because the kernel has already seen workers in
this phase. Touch the sentinel, restart the supervisor — the next
worker burst is fresh, then everything resumes normally.
"""


def _load_persisted_thread_id(
    work_dir: Path, role: str, session_scope: str, config: ProviderConfig
) -> str:
    if FORCE_FRESH_NEXT_SENTINEL.exists():
        try:
            FORCE_FRESH_NEXT_SENTINEL.unlink()
        except OSError:
            pass
        return ""
    path = _session_state_path(work_dir, role, session_scope, config)
    if not path.exists():
        return ""
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return ""
    if not isinstance(data, dict):
        return ""
    return str(data.get("thread_id", "") or "").strip()


def _store_persisted_thread_id(
    work_dir: Path,
    role: str,
    session_scope: str,
    config: ProviderConfig,
    thread_id: str,
) -> None:
    thread_id = str(thread_id or "").strip()
    if not thread_id:
        return
    path = _session_state_path(work_dir, role, session_scope, config)
    path.write_text(json.dumps({"thread_id": thread_id}, indent=2) + "\n", encoding="utf-8")


def _clear_persisted_thread_id(
    work_dir: Path, role: str, session_scope: str, config: Optional[ProviderConfig] = None
) -> None:
    path = _session_state_path(work_dir, role, session_scope, config)
    path.unlink(missing_ok=True)


def _extract_thread_id(output: str) -> str:
    for line in output.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            continue
        if rec.get("type") == "thread.started":
            return str(rec.get("thread_id", "") or "").strip()
    return ""


def _detected_context_overflow(output: str) -> bool:
    """Detect a codex `context_length_exceeded` failure in the burst output.

    When a long-running supervisor reuses the same codex thread across many
    dispatches via `codex exec resume <thread-id>`, the conversation history
    accumulates inside the codex session — even across failed-then-retried
    turns. Eventually the history alone exceeds the model's context window
    and every subsequent dispatch on that thread errors with:

        {"type":"error","message":"Error running remote compact task: ...
        context_length_exceeded ..."}
        {"type":"turn.failed", ...}

    The thread is permanently broken at this point — the only recovery is to
    abandon it and start a fresh thread. This helper detects the pattern in
    the streamed JSON output so the caller can skip persisting the now-bad
    thread id and force the next dispatch to start fresh.

    Failure mode this guards against: a long-lived thread that has
    accumulated too much context starts returning `context_length_exceeded`
    on every turn. If the supervisor keeps storing the same thread id and
    re-resuming, it loops indefinitely into the same wedged session. The
    only manual recovery is deleting the cached
    `.trellis/sessions/codex-<role>-*.json` file to force a fresh thread;
    this helper automates that detection so no operator intervention is
    needed.
    """
    # Codex emits multiple message shapes for "this thread is wedged":
    #   1. The model's own error: "context_length_exceeded" / "exceeds the
    #      context window" — surfaces when the API rejects the request.
    #   2. The codex CLI's user-facing fallback: "Codex ran out of room in
    #      the model's context window. Start a new thread or clear earlier
    #      history before retrying." — surfaces when the codex client
    #      pre-empts the API call after recognizing the situation locally.
    # Both indicate the same underlying problem (accumulated session history
    # > model context window) and the same recovery (start a fresh thread).
    overflow_phrases = (
        "context_length_exceeded",
        "exceeds the context window",
        "ran out of room in the model's context window",
    )

    def _matches_overflow(text: str) -> bool:
        return any(phrase in text for phrase in overflow_phrases)

    for line in output.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            continue
        rec_type = rec.get("type", "")
        if rec_type not in ("error", "turn.failed"):
            continue
        message = rec.get("message", "")
        if isinstance(message, str) and _matches_overflow(message):
            return True
        error = rec.get("error")
        if isinstance(error, dict):
            inner_message = error.get("message", "")
            if isinstance(inner_message, str) and _matches_overflow(inner_message):
                return True
    return False


def _should_persist_thread_id(role: str, fresh: bool) -> bool:
    """Persist worker threads across cycles and keep verification from clobbering reviewer state."""
    if role == "worker" and not fresh:
        return True
    if role == "reviewer" and not fresh:
        return True
    return False


def _persistent_context_scope(role: str, session_scope: str) -> bool:
    if role == "worker":
        return True
    if role != "reviewer":
        return False
    parts = [part for part in str(session_scope or "").split(":") if part]
    return len(parts) >= 3 and parts[2] == "review"


def build_script(
    config: ProviderConfig,
    *,
    prompt_file: Path,
    start_file: Path,
    exit_file: Path,
    work_dir: Path,
    burst_home: Optional[Path] = None,
    log_prefix: str = "worker",
    fresh: bool = False,
    resume_thread_id: str = "",
    use_ephemeral: bool = False,
) -> Path:
    """Generate a bash script that wraps the codex exec command."""
    cmd_parts = ["codex", "exec"]
    if resume_thread_id and not fresh:
        cmd_parts.extend(["resume"])
    cmd_parts.extend([
        "--json",
        "--skip-git-repo-check",
        "--dangerously-bypass-approvals-and-sandbox",
        # codex 0.136.0 probabilistically tries to register an image_generation
        # tool whose backing model (gpt-image-2) the account lacks access to,
        # causing a 400 at session warmup that exits codex with no artifact.
        # Trellis never asks for images; disable the tool so bursts don't die
        # at random.
        "--disable", "image_generation",
    ])
    if use_ephemeral:
        cmd_parts.append("--ephemeral")
    model = getattr(config, "model", "")
    effort = getattr(config, "effort", "")
    extra_args = getattr(config, "extra_args", None)
    if model:
        cmd_parts.extend(["-m", model])
    if effort:
        cmd_parts.extend(["-c", f"reasoning_effort={shlex.quote(effort)}"])
    cmd_parts.extend(extra_args or [])
    if resume_thread_id and not fresh:
        cmd_parts.append(resume_thread_id)
    cmd_parts.append("-")

    home_path = Path(burst_home) if burst_home is not None else None
    env_lines = [
        f"export PATH={shlex.quote(worker_path_env(home_path))}",
        f"export ELAN_HOME={shlex.quote(str(worker_elan_home()))}",
        "export PYTHONDONTWRITEBYTECODE=1",
    ]
    if home_path is not None:
        env_lines.append(f"export HOME={shlex.quote(str(home_path))}")
    for key in (
        "ANTHROPIC_API_KEY",
        *gemini_api_env_keys_to_forward(burst_home=burst_home),
        "OPENAI_API_KEY",
    ):
        val = os.environ.get(key)
        if val:
            env_lines.append(f"export {key}={shlex.quote(val)}")

    lines = [
        "#!/usr/bin/env bash",
        "set -u",
        "umask 0002",
        f"START_FILE={shlex.quote(str(start_file))}",
        f"EXIT_FILE={shlex.quote(str(exit_file))}",
        f"PROMPT_FILE={shlex.quote(str(prompt_file))}",
        f"WORK_DIR={shlex.quote(str(work_dir))}",
        "",
        "cleanup() {",
        "  ec=$?",
        "  trap - EXIT HUP INT TERM",
        "  if [[ -n \"${AGENT_PID:-}\" ]]; then",
        "    kill -- -\"$AGENT_PID\" 2>/dev/null || true",
        "    for _ in 1 2 3 4 5; do",
        "      if ! kill -0 -- -\"$AGENT_PID\" 2>/dev/null; then",
        "        break",
        "      fi",
        "      sleep 1",
        "    done",
        "    kill -KILL -- -\"$AGENT_PID\" 2>/dev/null || true",
        "    wait \"$AGENT_PID\" 2>/dev/null || true",
        "  fi",
        "  printf '%s\\n' \"$ec\" > \"$EXIT_FILE\"",
        "  exit \"$ec\"",
        "}",
        "trap cleanup EXIT HUP INT TERM",
        "",
        *env_lines,
        "",
        'cd "$WORK_DIR"',
        'printf "%s\\n" "$(date -Is)" > "$START_FILE"',
        "cmd=(",
        *[f"  {shlex.quote(p)}" for p in cmd_parts],
        ")",
        "",
        f'LOG_FILE={shlex.quote(str(start_file.parent / f"{log_prefix}-output.log"))}',
        'setsid "${cmd[@]}" < "$PROMPT_FILE" > "$LOG_FILE" 2>&1 &',
        'AGENT_PID=$!',
        'wait "$AGENT_PID"',
        "ec=$?",
        'exit "$ec"',
    ]

    script_path = start_file.parent / f"{log_prefix}-burst.sh"
    script_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    script_path.chmod(0o755)
    return script_path


def build_launcher_script(
    *,
    script_path: Path,
    launch_cmd: list[str],
    log_dir: Path,
    log_prefix: str,
) -> Path:
    # Post-bwrap-only: no sudo wrap; runs as supervisor inside bwrap.
    command = list(launch_cmd)
    lines = [
        "#!/usr/bin/env bash",
        "set -euo pipefail",
        "exec \\",
    ]
    lines.extend(f"  {shlex.quote(part)} \\" for part in command[:-1])
    lines.append(f"  {shlex.quote(command[-1])}")
    launcher_path = log_dir / f"{log_prefix}-launcher.sh"
    launcher_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    launcher_path.chmod(0o755)
    return launcher_path


def run(
    config: ProviderConfig,
    prompt: str,
    *,
    role: str = "worker",
    session_name: str,
    work_dir: Path,
    session_scope: str = "",
    startup_timeout: float = 3600.0,
    burst_timeout: float = 7200.0,
    log_dir: Optional[Path] = None,
    artifact_prefix: Optional[str] = None,
    fresh: bool = False,
    done_file: Optional[Path] = None,
    sandbox: Optional[SandboxConfig] = None,
    burst_home: Optional[Path] = None,
) -> BurstResult:
    """Run a Codex burst via the script-based pattern."""
    start = time.monotonic()
    start_wall_ms = int(time.time() * 1000)

    if log_dir is None:
        log_dir = work_dir / ".trellis" / "logs" / "bursts"
    log_dir.mkdir(parents=True, exist_ok=True)

    # Quota probe (start-of-burst). Forced (NOT cooldown-gated) and dispatched
    # onto a thread pool so it runs IN PARALLEL with the burst itself —
    # its 10-25s wall cost adds 0 to a 30-min worker burst. Wrapped so a
    # failure here NEVER stops a burst; quota tracking is cosmetic.
    pre_probe_future = None
    try:
        from trellis.agents.tmux_backend import _submit_probe_for_burst as _sub_probe  # noqa
        pre_probe_future = _sub_probe(
            "codex", work_dir, burst_home=burst_home,
        )
    except Exception:
        pre_probe_future = None
    prefix = _artifact_prefix(artifact_prefix, role)

    prompt_file = ensure_chat_file_link(
        work_dir,
        log_dir=log_dir,
        artifact_prefix=prefix,
        role=role,
        log_filename=f"{prefix}-prompt.txt",
        canonical_name="prompt.txt",
    )
    prompt_file.write_text(prompt, encoding="utf-8")
    prompt_file.chmod(0o644)

    start_file = log_dir / f"{prefix}.started"
    exit_file = log_dir / f"{prefix}.exit"
    start_file.unlink(missing_ok=True)
    exit_file.unlink(missing_ok=True)

    output_log = ensure_chat_file_link(
        work_dir,
        log_dir=log_dir,
        artifact_prefix=prefix,
        role=role,
        log_filename=f"{prefix}-output.log",
        canonical_name="output.log",
    )
    output_log.write_text("", encoding="utf-8")

    resume_thread_id = ""
    persistent_scope = _persistent_context_scope(role, session_scope)
    if role in {"worker", "reviewer"} and persistent_scope:
        if fresh:
            _clear_persisted_thread_id(work_dir, role, session_scope, config)
        else:
            resume_thread_id = _load_persisted_thread_id(
                work_dir, role, session_scope, config
            )

    script_path = build_script(
        config,
        prompt_file=prompt_file,
        start_file=start_file,
        exit_file=exit_file,
        work_dir=work_dir,
        burst_home=burst_home,
        log_prefix=prefix,
        fresh=fresh,
        resume_thread_id=resume_thread_id,
        # Drop --ephemeral universally. Empirically the codex CLI's
        # --ephemeral flag suppresses rollout-file writes entirely
        # (~/.codex/sessions/.../rollout-*.jsonl), which kills our
        # rollout-derived per-burst LLM-time signal for verifier kinds
        # (paper / corr / sound). Without --ephemeral the burst still
        # starts without prior-thread context (no `resume` arg unless
        # `resume_thread_id` was loaded from disk above, which only
        # happens for persistent_scope roles). Cost: ~/.codex/sessions/
        # grows unbounded; periodic prune is cheap if needed.
        use_ephemeral=False,
    )

    # Launch via tmux for process isolation
    from trellis.burst import tmux_ensure_session, tmux_kill_window, tmux_cmd, tmux_pane_is_dead
    tmux_ensure_session(session_name)
    window_name = f"{prefix}-codex"
    try:
        tmux_kill_window(session_name, window_name)
    except Exception:
        pass
    time.sleep(0.5)

    proc = tmux_cmd("new-window", "-d", "-P", "-F", "#{window_id} #{pane_id}",
                     "-t", session_name, "-n", window_name)
    if proc.returncode != 0:
        return BurstResult(ok=False, exit_code=None, captured_output="",
                          duration_seconds=time.monotonic() - start,
                          error=f"Failed to create tmux window: {proc.stderr}")
    window_id, pane_id = proc.stdout.strip().split()
    tmux_cmd("set-window-option", "-t", window_id, "remain-on-exit", "on")

    sandbox_cmd = wrap_command(
        [str(script_path)],
        sandbox=sandbox,
        work_dir=work_dir,
        burst_home=burst_home,
        role=role,
    )
    launcher_path = build_launcher_script(
        script_path=script_path,
        launch_cmd=sandbox_cmd,
        log_dir=log_dir,
        log_prefix=prefix,
    )
    launch_proc = tmux_cmd("send-keys", "-t", pane_id, str(launcher_path), "C-m")
    if launch_proc.returncode != 0:
        tmux_cmd("kill-window", "-t", window_id, check=False)
        return BurstResult(
            ok=False,
            exit_code=None,
            captured_output="",
            duration_seconds=time.monotonic() - start,
            error=f"Failed to launch agent window: {launch_proc.stderr}",
        )

    # Wait for start marker
    deadline_start = time.monotonic() + startup_timeout
    while time.monotonic() < deadline_start:
        if start_file.exists():
            break
        if tmux_pane_is_dead(pane_id):
            return BurstResult(ok=False, exit_code=None,
                              captured_output=output_log.read_text(errors="replace") if output_log.exists() else "",
                              duration_seconds=time.monotonic() - start,
                              error="Agent pane died before startup")
        time.sleep(0.5)
    if not start_file.exists():
        tmux_cmd("kill-window", "-t", window_id, check=False)
        return BurstResult(
            ok=False,
            exit_code=None,
            captured_output=output_log.read_text(errors="replace") if output_log.exists() else "",
            duration_seconds=time.monotonic() - start,
            error=f"Agent failed to create startup marker before timeout ({startup_timeout}s)",
        )

    # Wait for exit marker. Completion is unbounded; only startup is timed.
    done_seen_at: Optional[float] = None
    completed_via_done = False
    while True:
        if exit_file.exists():
            break
        if done_file is not None and done_file.exists():
            if done_seen_at is None:
                done_seen_at = time.monotonic()
            if tmux_pane_is_dead(pane_id):
                completed_via_done = True
                break
            if time.monotonic() - done_seen_at >= DONE_FILE_EXIT_GRACE_SECONDS:
                completed_via_done = True
                break
        if tmux_pane_is_dead(pane_id):
            time.sleep(2)
            if exit_file.exists():
                break
            if done_file is not None and done_file.exists():
                completed_via_done = True
                break
            return BurstResult(ok=False, exit_code=None,
                              captured_output=output_log.read_text(errors="replace") if output_log.exists() else "",
                              duration_seconds=time.monotonic() - start,
                              error="Agent pane died before exit")
        time.sleep(1)

    # If we got here via done_file, give codex a window to finish streaming
    # the closing `turn.completed` event before we kill the pane. Without
    # this, the cost ledger entry has `usage: null` because codex was
    # killed mid-turn-close. Poll output.log for the marker and exit early
    # as soon as it appears.
    if completed_via_done and not exit_file.exists():
        wait_started = time.monotonic()
        while time.monotonic() - wait_started < DONE_FILE_TURN_COMPLETED_WAIT_SECONDS:
            if exit_file.exists():
                break
            try:
                tail_text = output_log.read_text(errors="replace") if output_log.exists() else ""
            except Exception:
                tail_text = ""
            if '"type":"turn.completed"' in tail_text:
                break
            if tmux_pane_is_dead(pane_id):
                break
            time.sleep(1)
        tmux_cmd("kill-window", "-t", window_id, check=False)

    # Read result
    exit_code_text = exit_file.read_text().strip() if exit_file.exists() else ("0" if completed_via_done else "1")
    try:
        exit_code = int(exit_code_text)
    except ValueError:
        exit_code = 1

    time.sleep(0.5)
    output = output_log.read_text(errors="replace") if output_log.exists() else ""

    tmux_cmd("kill-window", "-t", window_id, check=False)

    # Parse usage from Codex JSON output (turn.completed event)
    usage = None
    for line in output.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
            if rec.get("type") == "turn.completed" and "usage" in rec:
                usage = rec["usage"]
                usage["provider"] = "codex"
                usage["model"] = config.model or "codex"
        except (json.JSONDecodeError, ValueError):
            pass

    thread_id = _extract_thread_id(output)
    context_overflow = _detected_context_overflow(output)
    if context_overflow and _persistent_context_scope(role, session_scope):
        # The codex thread is wedged: its accumulated conversation history
        # alone exceeds the model context window, so resuming it again will
        # just hit the same error. Clear the cached id so the next dispatch
        # starts a fresh thread. The current burst still returns ok=False
        # (exit_code != 0); the kernel's malformed-response retry path will
        # re-issue, and the re-issued request will see no cached thread and
        # launch a brand-new codex session.
        _clear_persisted_thread_id(work_dir, role, session_scope, config)
    elif thread_id and _persistent_context_scope(role, session_scope):
        _store_persisted_thread_id(work_dir, role, session_scope, config, thread_id)

    duration = time.monotonic() - start

    # Record this burst in the cost-ledger so per-provider reporting has a row
    # for codex like it does for claude/gemini. codex_cost_usd handles the
    # OpenAI Responses-API convention where input_tokens is gross (includes
    # cached_input_tokens).
    #
    # codex sessions resume across many bursts (`codex exec resume <thread>`),
    # and each `turn.completed` event reports usage CUMULATIVE-from-session-
    # start, not per-turn. So `cost` here is the running session total. To
    # avoid summing across rows over-counting the same carried context, we
    # pass it as `cumulative_cost_usd=` and let `append_cost_ledger` compute
    # the per-burst delta against the prior row for this `session_id` — same
    # pattern claude and gemini already use. Both `cost_usd` and `usage` get
    # delta'd; the cumulative snapshot lands in `session_total_*` for debug.
    #
    # Per-burst quota attribution: kick off a post-burst probe (forced) and
    # await both pre/post with short timeouts before writing the row. Either
    # may resolve to None — that just means the row records `quota_*: None`.
    try:
        from trellis.agents.tmux_backend import (
            append_cost_ledger,
            codex_cost_usd,
            _count_codex_turns,
            _submit_probe_for_burst,
            _await_with_short_timeout,
            _project_quota_for_ledger,
        )
        cumulative_cost = codex_cost_usd(usage) if isinstance(usage, dict) else None
        turn_count = _count_codex_turns(output)
        try:
            post_probe_future = _submit_probe_for_burst(
                "codex", work_dir, burst_home=burst_home,
            )
        except Exception:
            post_probe_future = None
        pre_payload = _await_with_short_timeout(pre_probe_future, 5.0)
        post_payload = _await_with_short_timeout(post_probe_future, 30.0)
        quota_pre = _project_quota_for_ledger(pre_payload, "codex")
        quota_post = _project_quota_for_ledger(post_payload, "codex")
        append_cost_ledger(
            work_dir,
            provider="codex",
            role=role,
            scope=session_scope,
            model=config.model or "",
            usage=usage,
            cost_usd=None,
            cumulative_cost_usd=cumulative_cost,
            duration_seconds=duration,
            attempts=1,
            ok=(exit_code == 0),
            reason="",
            session_id=thread_id,
            ts_start=time.time() - duration,
            message_count=turn_count,
            extra={"exit_code": exit_code, "effort": getattr(config, "effort", "") or None},
            quota_pre=quota_pre,
            quota_post=quota_post,
        )
    except Exception:
        pass  # ledger recording is best-effort; never block burst return

    # Drop a call.json sidecar alongside output.log so the viewer has a single
    # place to read provider/model/session metadata for this burst. The
    # chat_artifact_dir is the same dir ensure_chat_file_link wrote output.log
    # into above.
    try:
        from trellis.chat_history import chat_artifact_dir as _chat_artifact_dir
        artifact_dir = _chat_artifact_dir(
            work_dir, log_dir=log_dir, artifact_prefix=prefix, role=role,
        )
        call_meta = {
            "provider": "codex",
            "model": config.model or "",
            "role": role,
            "session_id": thread_id or None,
            "started_at_ms": start_wall_ms,
            "ended_at_ms": int(time.time() * 1000),
            "request_id": None,
            "artifact_id": artifact_dir.name,
            "scope": session_scope,
        }
        (artifact_dir / "call.json").write_text(
            json.dumps(call_meta, indent=2) + "\n", encoding="utf-8"
        )
    except Exception:
        pass

    # End-of-burst quota probe was already kicked off above and the cost
    # ledger row was stamped with the bracketed pre/post payloads. No extra
    # boundary probe needed here.

    # GATE H: surface a clear "provider CLI not found" cause instead of a
    # bare generic Malformed when the burst exits 127 because `codex` wasn't
    # on the sandbox PATH (install location not covered by worker_path_env).
    burst_error = ""
    if exit_code != 0:
        from trellis.host_runtime import provider_cli_not_found_detail
        not_found = provider_cli_not_found_detail(
            "codex",
            exit_code=exit_code,
            output=output,
            burst_home=Path(burst_home) if burst_home is not None else None,
        )
        if not_found:
            burst_error = not_found

    return BurstResult(
        ok=exit_code == 0,
        exit_code=exit_code,
        captured_output=output,
        duration_seconds=duration,
        usage=usage,
        error=burst_error,
    )
