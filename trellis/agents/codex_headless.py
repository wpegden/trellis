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
import random
import re
import shlex
import shutil
import subprocess
import time
from pathlib import Path
from typing import Optional

from trellis.adapters import BurstResult, ProviderConfig
from trellis.chat_history import ensure_chat_file_link
from trellis.provider_policy import is_policy_refusal_message, provider_error_messages
from trellis.config import SandboxConfig
from trellis.gemini_accounts import gemini_api_env_keys_to_forward
from trellis.git_trust import (launch_failure, report_directory, supervise_git_launch,
                               engineering_launch_status)
from trellis.host_runtime import (
    DEFAULT_WORKER_PATH,
    worker_elan_home,
    worker_isabelle_home,
    worker_path_env,
)
from trellis.sandbox import repo_targets_isabelle, wrap_command

WORKER_PATH = DEFAULT_WORKER_PATH
DONE_FILE_EXIT_GRACE_SECONDS = 2.0
# After done_file appears, give codex a longer window to finish its turn
# and emit `{"type":"turn.completed","usage":{...}}` to output.log. Without
# this we kill codex right after the done_file write, before it streams
# the closing turn — which loses usage/cost reporting in the cost ledger.
# Polled with early-exit on turn.completed; this is just the cap.
DONE_FILE_TURN_COMPLETED_WAIT_SECONDS = 60.0
# turn.completed publishes usage, but the CLI may still be flushing history.
TURN_COMPLETED_PROCESS_EXIT_GRACE_SECONDS = 2.0


def _has_turn_completed(output: str) -> bool:
    for line in output.splitlines():
        try:
            event = json.loads(line)
        except (ValueError, TypeError):
            continue
        if isinstance(event, dict) and event.get("type") == "turn.completed":
            return True
    return False


def _wait_for_process_exit(exit_file: Path, pane_id: str, tmux_query) -> None:
    deadline = time.monotonic() + TURN_COMPLETED_PROCESS_EXIT_GRACE_SECONDS
    while not exit_file.exists():
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        try:
            status = tmux_query("display-message", "-p", "-t", pane_id,
                                "#{pane_dead}", check=False, timeout=remaining)
        except subprocess.TimeoutExpired:
            break
        if status.returncode != 0 or status.stdout.strip() == "1":
            break
        time.sleep(min(0.1, max(0.0, deadline - time.monotonic())))


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
        if isinstance(rec, dict) and rec.get("type") == "thread.started":
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

    return any(_matches_overflow(message) for message in _codex_error_messages(output))


def _codex_error_messages(output: str) -> list[str]:
    return provider_error_messages(output)


def _unfinished_tool_host_failure(output: str, done_file: Optional[Path]) -> str:
    if done_file is None or done_file.exists():
        return ""
    # Match CLI diagnostics only, not agent prose or JSON tool output quoting
    # an error. A recovered burst with its completion marker stays on the
    # ordinary artifact-validation path.
    errors = re.findall(r"(?m)^\d{4}-\d\d-\d\dT\S+ ERROR codex_core::tools::router: error=(code-mode host (?:exited with status|closed its stdout)[^\n]*)$", output)
    return f"Codex tool runtime failed before completion: {errors[0]}" if errors else ""


def _detected_policy_refusal(output: str) -> bool:
    # The classifier judged the whole conversation, so the refused thread
    # must not be resumed: the next attempt (another model, or a later
    # request) starts fresh.
    return any(is_policy_refusal_message(m) for m in _codex_error_messages(output))


def _detected_compaction_bad_request(output: str) -> bool:
    # The provider's generic 400 does not prove context overflow. Retrying
    # from fresh context avoids reusing history rejected by compaction;
    # persistent provider failures remain subject to the bridge retry limit.
    return any(
        "error running remote compact task:" in message.lower()
        and "bad request" in message.lower()
        for message in _codex_error_messages(output)
    )


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


# --- A/B test: worker context policy (current resume policy vs always-fresh) ---
# OFF unless explicitly enabled, so normal use of this module is unaffected.
# Enable via env `TRELLIS_WORKER_CONTEXT_AB` (truthy) OR by touching the flag
# file `<work_dir>/.trellis/worker_context_ab.enabled` (read per burst, so the
# experiment can be toggled on a running supervisor without a restart). The
# treatment arm forces fresh context; the control arm leaves the kernel-decided
# `fresh` untouched. Each worker burst is assigned independently (default 50/50)
# and the choice is appended to `<work_dir>/.trellis/logs/worker_context_ab.jsonl`
# keyed by the burst's artifact prefix, so per-arm metrics (valid-burst rate,
# Lean LOC, ...) can be reconstructed from git and joined later.
WORKER_CONTEXT_AB_ENV = "TRELLIS_WORKER_CONTEXT_AB"
WORKER_CONTEXT_AB_PROB_ENV = "TRELLIS_WORKER_CONTEXT_AB_FRESH_PROB"
WORKER_CONTEXT_AB_FLAG_NAME = ".trellis/worker_context_ab.enabled"
WORKER_CONTEXT_AB_LOG_NAME = ".trellis/logs/worker_context_ab.jsonl"


def _truthy_env(value: str) -> bool:
    return str(value or "").strip().lower() in {"1", "true", "yes", "on"}


def _worker_context_ab_enabled(work_dir: Path) -> bool:
    if _truthy_env(os.environ.get(WORKER_CONTEXT_AB_ENV, "")):
        return True
    try:
        return (Path(work_dir) / WORKER_CONTEXT_AB_FLAG_NAME).exists()
    except OSError:
        return False


def _worker_context_ab_fresh_prob() -> float:
    raw = str(os.environ.get(WORKER_CONTEXT_AB_PROB_ENV, "") or "").strip()
    if not raw:
        return 0.5
    try:
        return min(1.0, max(0.0, float(raw)))
    except ValueError:
        return 0.5


def _record_worker_context_ab(work_dir: Path, record: dict) -> None:
    """Append one A/B decision record (best-effort; never raises)."""
    try:
        path = Path(work_dir) / WORKER_CONTEXT_AB_LOG_NAME
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(record, sort_keys=True) + "\n")
    except OSError:
        pass


def _decide_worker_context_ab_arm(role: str, work_dir: Path) -> str:
    """Return this worker burst's A/B arm: ``always_fresh`` (treatment),
    ``control``, or ``""`` when the experiment is disabled or the role is not
    worker — in which case the caller leaves behavior entirely unchanged.

    Independent per-burst draw at the configured probability (default 0.5).
    """
    if role != "worker" or not _worker_context_ab_enabled(work_dir):
        return ""
    return (
        "always_fresh"
        if random.random() < _worker_context_ab_fresh_prob()
        else "control"
    )


# Default worker context policy: always start a FRESH codex session (never
# resume the prior thread). A blinded productivity A/B (always-fresh vs the
# legacy resume policy) found fresh sessions meaningfully more productive, so
# this is the default. This is THREAD-ONLY: the kernel's `fresh_context` path
# (scratch-workspace reset, prompt context-mode hint, ...) is untouched — i.e.
# exactly the configuration that was measured. Reviewer/verifier roles are
# unaffected. Opt back into legacy resume with TRELLIS_WORKER_CONTEXT_LEGACY_RESUME=1
# (or TRELLIS_WORKER_ALWAYS_FRESH=0). The A/B experiment, when enabled, takes
# precedence over this default (so an experiment machine keeps randomizing).
LEGACY_RESUME_ENV = "TRELLIS_WORKER_CONTEXT_LEGACY_RESUME"
ALWAYS_FRESH_ENV = "TRELLIS_WORKER_ALWAYS_FRESH"


def _worker_always_fresh_enabled() -> bool:
    if _truthy_env(os.environ.get(LEGACY_RESUME_ENV, "")):
        return False
    override = os.environ.get(ALWAYS_FRESH_ENV, "")
    if override != "":
        return _truthy_env(override)
    return True


BASE_INSTRUCTIONS_DIR = Path(__file__).resolve().parent / "codex_base_instructions"
WORKER_BASE_INSTRUCTIONS_SOURCE = BASE_INSTRUCTIONS_DIR / "worker.md"


def _sanitize_base_instructions_key(key: str) -> str:
    """Restrict the per-role key to a safe filename stem.

    The key selects `codex_base_instructions/<key>.md`; sanitizing keeps it
    a single path component (no traversal, no separators) so an unexpected
    value can only ever map to a missing file (→ stock prompt), never escape
    the base-instructions directory.
    """
    cleaned = re.sub(r"[^A-Za-z0-9_.-]+", "_", str(key or "")).strip("._-")
    return cleaned[:80]


def _materialize_base_instructions(
    burst_home: Path, key: str
) -> Optional[Path]:
    """Copy the per-role base-instructions file for ``key`` into the
    burst-home and return its absolute path (which resolves identically
    inside the bwrap sandbox).

    The source is ``codex_base_instructions/<key>.md``. Returns None when
    ``key`` is empty or the source file is missing, so the caller falls back
    to codex's stock base_instructions rather than passing a dangling path
    (codex errors on an unreadable model_instructions_file). File presence is
    therefore the only switch: adding/removing a role prompt is just adding or
    removing the matching ``<key>.md``.
    """
    stem = _sanitize_base_instructions_key(key)
    if not stem:
        return None
    src = BASE_INSTRUCTIONS_DIR / f"{stem}.md"
    try:
        if not src.is_file():
            return None
    except OSError:
        return None
    from trellis.agents.host_files import atomic_text, provider_directory
    try:
        with provider_directory(burst_home) as (directory, fd):
            return atomic_text(directory, fd, f"trellis-{stem}-base-instructions.md", src.read_text())
    except OSError as error:
        raise RuntimeError(f"Cannot prepare Codex base instructions: {error}") from error


def codex_config_args(config: ProviderConfig) -> list[str]:
    """Apply Trellis defaults and model settings, then explicit caller overrides."""
    args: list[str] = ["-c", "features.tool_suggest=false", "-c", "model_reasoning_summary=auto"]
    model = getattr(config, "model", "")
    effort = getattr(config, "effort", "")
    if model:
        args.extend(["-m", model])
    if effort:
        args.extend(["-c", f"model_reasoning_effort={effort}"])
    return args + list(getattr(config, "extra_args", None) or [])


def _engineering_text(path, text, *, mode=0o600):
    from trellis.agents.host_files import safe_directory, atomic_text
    with safe_directory(path.parent) as (directory, fd):
        return atomic_text(directory, fd, path.name, text, mode=mode)


def _engineering_read(path, *, optional=False):
    from trellis.agents.engineering_exec import open_regular
    try:
        with open_regular(path) as stream:
            if os.fstat(stream.fileno()).st_nlink != 1:
                raise ValueError('Aliased readable evidence')
            return stream.read().decode('utf-8', errors='replace')
    except FileNotFoundError:
        if optional:
            return None
        raise RuntimeError('Engineering readable evidence invalid') from None
    except (OSError, ValueError):
        raise RuntimeError('Engineering readable evidence invalid') from None


def build_script(
    config: ProviderConfig,
    *,
    prompt_file: Path,
    start_file: Path,
    exit_file: Path,
    work_dir: Path,
    burst_home: Optional[Path] = None,
    log_prefix: str = "worker",
    provider_start_marker: str = "",
    fresh: bool = False,
    resume_thread_id: str = "",
    use_ephemeral: bool = False,
    role: str = "worker",
    base_instructions_key: Optional[str] = None,
    engineering_request: Optional[dict] = None,
    engineering_files: tuple = (),
) -> Path:
    """Generate a bash script that wraps the codex exec command.

    ``base_instructions_key`` selects which per-role base-instructions file
    (``codex_base_instructions/<key>.md``) overrides codex's stock prompt for
    this burst. When omitted it falls back to ``role``, preserving the worker
    default (``role="worker"`` → ``worker.md``). A key whose file is absent
    falls back to codex's stock prompt with no override.
    """
    from scripts.paper_provider_runtime import admit_burst
    engineering = role == "engineering"
    if engineering:
        from trellis.agents.engineering_context import validate
        if (burst_home is None or base_instructions_key != "engineering"
                or (not engineering_request and (not fresh or resume_thread_id))):
            raise ValueError("Engineering Codex requires fresh context and engineering instructions")
        validate(burst_home, config, request=engineering_request)
    admission = admit_burst(config, work_dir, burst_home, execution_kind="engineering" if engineering else "headless")
    if engineering:
        from trellis.agents.model_preflight import codex_runtime_identity, check_engineering_features
        identity = (admission["runtime"] if admission is not None else
                    codex_runtime_identity(work_dir, burst_home, execution_kind="engineering"))
        check_engineering_features(dict(work_dir=work_dir, burst_home=burst_home, identity=identity,
                                        execution_kind="engineering", role="engineering", environment=dict(os.environ),
                                        engineering_request=engineering_request, engineering_files=engineering_files))
    if admission is not None:
        evidence = start_file.with_name(start_file.name + ".provider.json")
        data = json.dumps(admission, sort_keys=True, indent=2) + "\n"
        if engineering:
            from trellis.agents.host_files import safe_directory
            from trellis.agents.engineering_exec import open_regular
            with safe_directory(evidence.parent) as (_, directory_fd):
                try:
                    fd = os.open(evidence.name, os.O_WRONLY | os.O_CREAT | os.O_EXCL |
                                 os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=directory_fd)
                except FileExistsError:
                    with open_regular(evidence) as stream:
                        if stream.read() != data.encode():
                            raise ValueError("Provider runtime evidence changed for an existing burst")
                else:
                    with os.fdopen(fd, 'w') as stream:
                        stream.write(data)
                        stream.flush()
                        os.fsync(stream.fileno())
                    os.fsync(directory_fd)
        elif evidence.exists():
            if evidence.read_text() != data:
                raise ValueError("Provider runtime evidence changed for an existing burst")
        else:
            with evidence.open("x") as stream:
                stream.write(data)
    cmd_parts = [admission["runtime"]["resolved_executable"] if admission is not None else "codex", "exec"]
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
    if engineering:
        cmd_parts.remove("--dangerously-bypass-approvals-and-sandbox")
    # Raise codex's empty-poll ceiling so one write_stdin can wait out a long
    # `lake build` / Trellis checker instead of the model re-polling every ~30s
    # (each poll re-sends the whole prefix). codex default is 300000ms (5min);
    # 600000ms (10min) spans a typical checker. extra_args below can override.
    cmd_parts.extend(["-c", "background_terminal_max_timeout=600000"])
    # Emit `reasoning` items into the --json stream (section-header summaries
    # of what the model attended to, in order). Off by default in codex, so
    # without this the burst log carries only agent_message/command_execution
    # and the attention sequence is unrecoverable after the fact. Cheap:
    # a few short strings per burst. extra_args below can override.
    cmd_parts.extend(["-c", "model_reasoning_summary=detailed"])
    # Per-role base-instructions override (worker.md, reviewer.md, the verifier
    # lanes, stuck_math_audit.md, ...). Generic file-presence lookup: the key
    # → `codex_base_instructions/<key>.md`, materialized into the burst-home
    # (the source tree is not bind-mounted into the burst bwrap, but the
    # burst-home is). A key with no matching file falls back to codex's stock
    # prompt with no override.
    instr_key = base_instructions_key if base_instructions_key is not None else role
    if instr_key and burst_home is not None:
        instr_path = _materialize_base_instructions(Path(burst_home), instr_key)
        if engineering and (instr_path is None or not instr_path.read_text().strip()):
            raise ValueError("Engineering base instructions are unavailable")
        if instr_path is not None:
            cmd_parts.extend(["-c", f"model_instructions_file={instr_path}"])
    if use_ephemeral:
        cmd_parts.append("--ephemeral")
    cmd_parts.extend(codex_config_args(config))
    from trellis.sandbox import _MANAGED_CHECK_ROLES
    if role in _MANAGED_CHECK_ROLES and (os.environ.get('TRELLIS_COMPILER_DATA_VIEW') or os.environ.get('TRELLIS_COMPILER_BWRAP')) and os.environ.get('TRELLIS_AGENT_COMPILER_CAPABILITY') in ('managed', 'none'):
        if burst_home is None:
            raise RuntimeError('Compiler-confined Codex requires an isolated burst home')
        from trellis.agents.codex_tool_mode import direct_tool_catalog
        catalog = direct_tool_catalog(config, burst_home, work_dir, role=role)
        cmd_parts.extend(['-c', f'model_catalog_json={catalog}'])
    if resume_thread_id and not fresh:
        cmd_parts.append(resume_thread_id)
    cmd_parts.append("-")

    home_path = Path(burst_home) if burst_home is not None else None
    # On an isabelle_hol run the codex burst's interactive proof loop needs
    # `isabelle` on PATH (the isabelle bin dir is also RO-bound by
    # `wrap_command`). Gated on the repo's `default_target`; a Lean run leaves
    # the PATH unchanged (byte-identical).
    env_lines = []
    if not engineering:
        _isabelle_run = repo_targets_isabelle(work_dir)
        env_lines = [
            f"export PATH={shlex.quote(worker_path_env(home_path, isabelle=_isabelle_run))}",
            'if [ -n "${TRELLIS_SOURCE_PATH:-}" ]; then export PATH="$TRELLIS_SOURCE_PATH:$PATH"; fi',
            'if [ -n "${TRELLIS_CHECK_JOBS_SOCKET:-}" ] || [ "${TRELLIS_COMPILER_CAPABILITY:-}" = managed ] || [ "${TRELLIS_COMPILER_CAPABILITY:-}" = none ]; then export PATH=' + shlex.quote(str(work_dir / ".trellis/scripts/check-bin")) + ':"$PATH"; fi',
            f"export ELAN_HOME={shlex.quote(str(worker_elan_home()))}",
            "export PYTHONDONTWRITEBYTECODE=1",
        ]
        if _isabelle_run:
            # `.trellis/scripts/isa-query` resolves the launcher as
            # `${TRELLIS_ISABELLE_BIN:-isabelle}`, so exporting PATH alone leaves it
            # depending on PATH surviving whatever shell the agent happens to use
            # (codex runs commands through `bash -lc`). Live consequence, conn-isa
            # cycle 73: the stuck-math audit reported "no Isabelle binary was on
            # PATH and TRELLIS_ISABELLE_BIN was unset" — isa-query's own guard —
            # and completed the audit without library discovery, degraded to source
            # inspection and the paper.
            #
            # Exporting the var makes the resolution independent of PATH. Same
            # source of truth as the PATH entry (`worker_isabelle_home()`), so the
            # two cannot drift. Gated on the isabelle target exactly as the PATH
            # prepend is, so a Lean run's burst env stays byte-identical.
            env_lines.append(
                "export TRELLIS_ISABELLE_BIN="
                f"{shlex.quote(str(worker_isabelle_home() / 'bin' / 'isabelle'))}"
            )
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
        # The script's own stderr (a failed redirect, a missing binary, a
        # sandbox refusal) otherwise lands only in the tmux pane and is gone
        # with the window; keep it beside the exit marker so the caller can
        # quote it when the agent never produced output.
        f"BURST_STDERR={shlex.quote(str(start_file.parent / f'{log_prefix}-burst.stderr'))}",
        'exec 2>>"$BURST_STDERR"',
        "",
        *env_lines,
        "",
        'cd "$WORK_DIR"',
        # ISSUE-2 burst-start prewarm hook (OFF by default). When the operator
        # sets TRELLIS_INCREMENTAL_PREWARM=1 AND names the active node in
        # TRELLIS_PREWARM_NODE, kick off a backgrounded, non-blocking
        # `incremental-check --prewarm <node>` so the warm `lean --server`
        # pays its cold start before the worker's first edit. Fully guarded:
        # it runs only when both env vars are set and the wrapper exists, is
        # detached so it never delays the burst, and the wrapper itself is
        # failure-open (always exits 0). Harmless when anything is missing.
        '# --- advisory incremental-check prewarm (disabled unless flagged) ---',
        'if [[ "${TRELLIS_INCREMENTAL_PREWARM:-0}" == "1" && -n "${TRELLIS_PREWARM_NODE:-}" ]]; then',
        '  _ic="$WORK_DIR/.trellis/scripts/incremental-check"',
        '  if [[ -x "$_ic" ]]; then',
        '    ( "$_ic" --prewarm "$TRELLIS_PREWARM_NODE" >/dev/null 2>&1 & ) || true',
        '  fi',
        'fi',
        '# --- end prewarm hook ---',
        'printf "%s\\n" "$(date -Is)" > "$START_FILE"',
        "cmd=(",
        *[f"  {shlex.quote(p)}" for p in cmd_parts],
        ")",
        "",
        f'LOG_FILE={shlex.quote(str(start_file.parent / f"{log_prefix}-output.log"))}',
        *([f"printf '%s\\n' {shlex.quote(provider_start_marker)}",
           'setsid "${cmd[@]}" < "$PROMPT_FILE" 2>&1 &'] if provider_start_marker else [
           'setsid "${cmd[@]}" < "$PROMPT_FILE" > "$LOG_FILE" 2>&1 &']),
        'AGENT_PID=$!',
        'wait "$AGENT_PID"',
        "ec=$?",
        'exit "$ec"',
    ]

    if role not in _MANAGED_CHECK_ROLES:
        first = lines.index('# --- advisory incremental-check prewarm (disabled unless flagged) ---')
        last = lines.index('# --- end prewarm hook ---')
        del lines[first:last + 1]
    script_path = start_file.parent / f"{log_prefix}-burst.sh"
    if engineering:
        _engineering_text(script_path, "\n".join(lines) + "\n", mode=0o755)
    else:
        script_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        script_path.chmod(0o755)
    return script_path


def build_launcher_script(
    *,
    script_path: Path,
    launch_cmd: list[str],
    log_dir: Path,
    log_prefix: str,
    provider_clock: Optional[Path] = None,
) -> Path:
    # Post-bwrap-only: no sudo wrap; runs as supervisor inside bwrap.
    command = supervise_git_launch(list(launch_cmd))
    from trellis.git_trust import engineering_launch
    engineering = engineering_launch(launch_cmd)
    from trellis.agents.engineering_context import launch_guards
    guards = launch_guards(list(launch_cmd))
    if provider_clock is not None:
        import sys
        if engineering:
            from trellis.agent_wrapper import provider_clock as clock
            command = ['/usr/bin/python3', '-I', '-S', str(Path(clock.__file__).resolve()),
                       '--engineering-owned', str(provider_clock), '--', *command]
        else:
            command = [sys.executable, '-m', 'trellis.agent_wrapper.provider_clock', str(provider_clock), '--', *command]
    lines = [
        "#!/usr/bin/env bash",
        "set -euo pipefail",
        *guards,
        "exec \\",
    ]
    lines.extend(f"  {shlex.quote(part)} \\" for part in command[:-1])
    lines.append(f"  {shlex.quote(command[-1])}")
    launcher_path = log_dir / f"{log_prefix}-launcher.sh"
    if engineering:
        _engineering_text(launcher_path, "\n".join(lines) + "\n", mode=0o755)
    else:
        launcher_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        launcher_path.chmod(0o755)
    return launcher_path


def _run_engineering_owned(launcher, command, *, work_dir, environment,
                           start_file, exit_file, startup_timeout, burst_timeout, clock_record=None, completion=None,
                           interruption=None):
    """Own one admitted payload; completion requires recorder and marker agreement."""
    from trellis.agents.engineering_exec import verify
    from trellis.pv_execution import owned_process
    import signal
    projected = verify(Path(command[5]), command[7], command[9:], environment)
    origin = time.monotonic()
    with owned_process(["/bin/bash", str(launcher)], cancel_grace=.5 if clock_record else 0, cwd=work_dir,
                       env=projected, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL) as process:
        try:
            while process.poll() is None:
                elapsed = time.monotonic() - origin
                if clock_record is None:
                    if elapsed >= burst_timeout or (not start_file.is_file() and elapsed >= startup_timeout):
                        raise TimeoutError("Engineering execution deadline reached")
                else:
                    from trellis.agent_wrapper import provider_clock
                    # The owned pipe arbitrates provider allowance. This outer
                    # bound only contains a stalled clock/bootstrap/cleanup.
                    if elapsed >= startup_timeout + burst_timeout + 130:
                        raise TimeoutError('Engineering clock cleanup deadline reached')
                time.sleep(.05)
            status = process.wait()
        except BaseException:
            # Let owned_process deliver cancellation to the clock's owner
            # before its bounded fallback kill.
            raise
        else:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
    deadline = time.monotonic() + 5
    terminal = engineering_launch_status(command)
    while terminal is None and time.monotonic() < deadline:
        time.sleep(.05)
        terminal = engineering_launch_status(command)
    evidence = dict(outer_returncode=status, binding_sha256=command[7],
                    launcher_receipt='missing' if terminal is None else 'invalid' if terminal[1] == 'execution_unknown' else 'valid')
    if terminal is not None:
        evidence.update(launcher_returncode=terminal[0], launcher_code=terminal[1])
    if clock_record is not None:
        from trellis.agent_wrapper import provider_clock
        from trellis.git_trust import engineering_completion
        records = {}
        evidence.update(clock_started='unobserved', clock_finished='unobserved', clock_exited='unobserved')
        try:
            for name in ('started', 'finished', 'exited'):
                try:
                    record = provider_clock.read(clock_record, name)
                except Exception:
                    evidence['clock_' + name] = 'invalid'
                    raise
                records[name] = record
                if record is None:
                    try:
                        (Path(clock_record) / (name + '.json')).lstat()
                    except FileNotFoundError:
                        evidence['clock_' + name] = 'missing'
                    except OSError:
                        evidence['clock_' + name] = 'invalid'
                    else:
                        evidence['clock_' + name] = 'invalid'
                else:
                    evidence['clock_' + name] = 'valid' if isinstance(record, dict) else 'invalid'
                if name == 'finished' and isinstance(record, dict):
                    if record.get('disposition') not in ('completed', 'interrupted', 'expired'):
                        evidence['clock_finished'] = 'invalid'
                    evidence.update(clock_disposition=record.get('disposition'), provider_error=record.get('provider_error'), clock_elapsed_ms=record.get('elapsed_ms'),
                                    principal_thread_id=record.get('thread_id'), clock_config_sha256=record.get('config_sha256'))
                    owner = record.get('owner')
                    if isinstance(owner, dict):
                        evidence.update(clock_owner_pid=owner.get('pid'), clock_owner_start_ticks=owner.get('start_ticks'),
                                        clock_owner_boot_id=owner.get('boot_id'))
                if name == 'exited' and isinstance(record, dict):
                    if type(record.get('returncode')) is not int or not -(2**31) <= record['returncode'] < 2**31:
                        evidence['clock_exited'] = 'invalid'
                    evidence['clock_returncode'] = record.get('returncode')
            started, finished, exited = (records[name] for name in ('started', 'finished', 'exited'))
            completed = bool(started and finished and exited and finished['disposition'] == 'completed'
                             and exited['returncode'] == 0)
            if (interruption is not None and started and finished and exited
                    and finished['disposition'] == 'interrupted'
                    and finished.get('provider_error') == 'server_overloaded' and finished.get('thread_id')
                    and type(exited.get('returncode')) is int
                    and terminal == (exited['returncode'] & 255, 'launcher_exited')
                    and status == ((exited['returncode'] or 124) & 255)):
                interruption.update(finished)
        except Exception as error:
            safe = engineering_completion(evidence)
            if completion is not None:
                completion.update(safe)
            error.engineering_completion = safe
            raise
    else:
        evidence.update(clock_started='not_required', clock_finished='not_required', clock_exited='not_required')
        completed = (_engineering_read(start_file, optional=True) is not None
                     and (_engineering_read(exit_file, optional=True) or '').strip() == '0')
    if completion is not None:
        from trellis.git_trust import engineering_completion
        completion.update(engineering_completion(evidence))
    return status == 0 and completed and terminal == (0, "launcher_exited")


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
    base_instructions_key: Optional[str] = None,
    require_turn_completion: bool = False,
    exact_resume_thread_id: str = "",
    session_record: Optional[Path] = None,
    execution_record: Optional[Path] = None,
) -> BurstResult:
    """Run a Codex burst via the script-based pattern.

    ``base_instructions_key`` selects the per-role base-instructions override
    (see ``build_script``); when omitted it falls back to ``role``.
    """
    native_binding, native_files, native_single, native_submission = None, (), None, None
    if role == "engineering":
        import threading
        from trellis.pv_execution import positive_timeout
        if threading.current_thread() is not threading.main_thread():
            raise ValueError("Engineering requires a fresh main-thread request")
        if require_turn_completion:
            from trellis.agents.engineering_context import request_binding, native_request
            if execution_record is None or burst_home is None:
                raise ValueError('Engineering submission requires an owned native turn')
            if os.path.lexists(execution_record / 'config.json'):
                raise RuntimeError('Engineering turn already prepared; retain its owned execution')
            native_binding, native_files = request_binding(work_dir, burst_home, execution_record)
            native_single, native_submission, _ = native_request(Path(native_binding['native_record']), work_dir, burst_home)
            if (config != native_single.provider or prompt != native_single.prompt or fresh != native_single.fresh
                    or exact_resume_thread_id != native_single.resume_thread_id or session_record != native_single.session_record
                    or burst_timeout != native_single.timeout_seconds or startup_timeout != native_single.startup_timeout_seconds
                    or session_name != native_single.session_name or session_scope != native_single.session_scope
                    or log_dir != native_single.log_dir or sandbox != native_single.sandbox):
                raise ValueError('Engineering invocation differs from its prepared native turn')
        elif not fresh or exact_resume_thread_id or session_record is not None:
            raise ValueError('Engineering continuation requires native submission authorization')
        startup_timeout = positive_timeout(startup_timeout)
        burst_timeout = positive_timeout(burst_timeout)
    start = time.monotonic()
    start_wall_ms = int(time.time() * 1000)
    if exact_resume_thread_id and not require_turn_completion:
        raise ValueError("exact worker continuation requires the submission lifecycle")
    if require_turn_completion:
        from trellis.agent_wrapper.continuation import require_session, session_binding
        if session_record is None or burst_home is None or burst_timeout <= 0:
            raise ValueError("worker submission requires retained context and a positive remaining allowance")
        # Reject a missing/different history before invoking the provider.
        if exact_resume_thread_id:
            require_session(session_record, session_binding(config, burst_home, exact_resume_thread_id))

    if log_dir is None:
        log_dir = work_dir / ".trellis" / "logs" / "bursts"
    if role == 'engineering':
        from trellis.agents.host_files import safe_directory
        with safe_directory(log_dir):
            pass
    else:
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
        **(dict(engineering=True) if role == 'engineering' else {}),
    )
    if role == 'engineering':
        _engineering_text(prompt_file, prompt, mode=0o644)
    else:
        prompt_file.write_text(prompt, encoding="utf-8")
        prompt_file.chmod(0o644)

    start_file = log_dir / f"{prefix}.started"
    exit_file = log_dir / f"{prefix}.exit"
    if role == 'engineering':
        with safe_directory(log_dir) as (_, fd):
            for marker in (start_file, exit_file):
                try:
                    os.unlink(marker.name, dir_fd=fd)
                except FileNotFoundError:
                    pass
    else:
        start_file.unlink(missing_ok=True)
        exit_file.unlink(missing_ok=True)

    output_log = ensure_chat_file_link(
        work_dir,
        log_dir=log_dir,
        artifact_prefix=prefix,
        role=role,
        log_filename=f"{prefix}-output.log",
        canonical_name="output.log",
        **(dict(engineering=True) if role == 'engineering' else {}),
    )

    # Worker context-policy A/B test (no-op unless explicitly enabled).
    # `always_fresh` (treatment) forces a fresh codex thread; `control` keeps
    # the kernel-decided `fresh`. When the treatment INJECTS freshness on a
    # burst current policy would have resumed (fresh_in is False), it must not
    # disturb the persisted control thread — neither clearing it here nor
    # storing its own thread after the burst (see `ab_injected_fresh` below) —
    # so control bursts keep resuming the genuine ongoing session. Only those
    # fresh_in==False bursts are the genuinely-random A/B; `fresh_in` is
    # recorded so the analysis can restrict statistics to them.
    fresh_in = bool(fresh)
    ab_arm = "" if require_turn_completion else _decide_worker_context_ab_arm(role, work_dir)
    ab_injected_fresh = ab_arm == "always_fresh" and not fresh_in
    if ab_arm == "always_fresh":
        fresh = True
    elif not require_turn_completion and not ab_arm and role == "worker" and _worker_always_fresh_enabled():
        # Default policy (no A/B running): worker always starts fresh. Thread-
        # only; scratch and the kernel fresh_context path are left as the kernel
        # decided. A/B (above) takes precedence so experiment machines randomize.
        fresh = True

    resume_thread_id = exact_resume_thread_id
    persistent_scope = _persistent_context_scope(role, session_scope)
    if require_turn_completion:
        fresh = not bool(exact_resume_thread_id)
    elif role in {"worker", "reviewer"} and persistent_scope:
        if fresh:
            if not ab_injected_fresh:
                _clear_persisted_thread_id(work_dir, role, session_scope, config)
        else:
            resume_thread_id = _load_persisted_thread_id(
                work_dir, role, session_scope, config
            )

    if ab_arm:
        _record_worker_context_ab(
            work_dir,
            {
                "ts_ms": int(time.time() * 1000),
                "burst_prefix": str(prefix or ""),
                "arm": ab_arm,
                # current policy's own decision for this burst; the A/B choice
                # is only genuinely random where this is False.
                "fresh_in": fresh_in,
                "fresh_effective": bool(fresh),
                "ab_injected_fresh": ab_injected_fresh,
                "thread_resumed": bool(resume_thread_id),
                "fresh_prob": _worker_context_ab_fresh_prob(),
                "session_scope": str(session_scope or ""),
            },
        )

    clock_config = None
    clock_root = None
    if require_turn_completion:
        from trellis.agent_wrapper import provider_clock
        execution_record = execution_record or session_record.parent / (prefix + '-execution')
        clock_root = execution_record
        clock_config = provider_clock.prepare(execution_record,
            allowance_ms=int(burst_timeout * 1000),
            binding=dict(provider=config.provider, model=config.model, reasoning_effort=config.effort,
                         home=str(burst_home.resolve()), thread_id=exact_resume_thread_id),
            output=output_log, startup_seconds=startup_timeout)
        execution_record = Path(clock_config.get('segment_directory', execution_record))
    if clock_config is None or not clock_config.get('append_output'):
        if role == 'engineering':
            _engineering_text(output_log, '')
        else:
            output_log.write_text("", encoding="utf-8")

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
        role=role,
        base_instructions_key=base_instructions_key,
        provider_start_marker=clock_config['marker'] if clock_config else '',
        **(dict(engineering_request=native_binding, engineering_files=tuple(native_files)) if native_binding else {}),
    )

    completed_via_done = False
    timing = None
    recorded_thread = ""
    def capture_session():
        nonlocal recorded_thread
        if not require_turn_completion or recorded_thread:
            return
        from trellis.agent_wrapper.continuation import record_session, session_binding
        receipt = provider_clock.read(execution_record, 'session') or provider_clock.read(execution_record, 'finished')
        thread = receipt.get('thread_id', '') if receipt else ''
        if not thread:
            return
        if exact_resume_thread_id and thread != exact_resume_thread_id:
            if role != 'engineering':
                from trellis.burst import tmux_kill_session
                tmux_kill_session(session_name)
            raise RuntimeError("worker correction resumed a different provider conversation")
        record_session(session_record, session_binding(config, burst_home, thread))
        recorded_thread = thread
    if role == "engineering":
        from trellis.agents.engineering_context import request_binding
        binding, files = (native_binding, native_files) if native_binding else request_binding(work_dir, burst_home)
        environment = dict(os.environ)
        sandbox_cmd = wrap_command(["/bin/bash", str(script_path)], sandbox=sandbox,
            work_dir=work_dir, burst_home=burst_home, role=role, environment=environment,
            engineering_request=binding, engineering_files=(*files, script_path, prompt_file))
        launcher_path = build_launcher_script(script_path=script_path, launch_cmd=sandbox_cmd,
            log_dir=log_dir, log_prefix=prefix, provider_clock=execution_record if require_turn_completion else None)
        completion = {}
        interruption = {}
        if not _run_engineering_owned(launcher_path, sandbox_cmd, work_dir=work_dir,
                environment=environment, start_file=start_file, exit_file=exit_file,
                startup_timeout=startup_timeout, burst_timeout=burst_timeout,
                clock_record=execution_record if require_turn_completion else None, completion=completion,
                interruption=interruption if require_turn_completion else None):
            if interruption:
                capture_session()
                from trellis.agents.engineering_context import retain_interrupted_session
                retain_interrupted_session(native_single, native_submission, sandbox_cmd)
                return BurstResult(ok=False, exit_code=completion.get('launcher_returncode'), captured_output='',
                    duration_seconds=time.monotonic() - start, error='server_overloaded',
                    provider_session_id=recorded_thread, provider_turn_completed=False,
                    provider_execution=interruption)
            return BurstResult(ok=False, exit_code=None, captured_output="",
                duration_seconds=time.monotonic() - start, error="engineering_execution_unknown",
                provider_execution=dict(completion=completion))
        if require_turn_completion:
            capture_session()
            from trellis.agents.engineering_context import retain_completed_session
            retain_completed_session(native_single, native_submission, sandbox_cmd)
    else:
        # Launch via tmux for process isolation
        from trellis.burst import tmux_ensure_session, tmux_kill_window, tmux_kill_session, tmux_cmd, tmux_pane_is_dead
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

        engineering_options = {}
        if role == "engineering":
            from trellis.agents.engineering_context import request_binding
            binding, files = request_binding(work_dir, burst_home)
            engineering_options = dict(engineering_request=binding,
                                       engineering_files=(*files, script_path, prompt_file))
        sandbox_cmd = wrap_command(
            ["/bin/bash", str(script_path)],
            sandbox=sandbox,
            work_dir=work_dir,
            burst_home=burst_home,
            role=role,
            # The inner script exports this PATH only after the shared preflight.
            git_preflight_path=(None if role == "engineering" else
                                worker_path_env(burst_home, isabelle=repo_targets_isabelle(work_dir))),
            **engineering_options,
        )
        launcher_path = build_launcher_script(
            script_path=script_path,
            launch_cmd=sandbox_cmd,
            log_dir=log_dir,
            log_prefix=prefix,
            provider_clock=execution_record if require_turn_completion else None,
        )
        launch_proc = tmux_cmd("send-keys", "-t", pane_id, shlex.quote(str(launcher_path)), "C-m")
        if launch_proc.returncode != 0:
            tmux_kill_session(session_name)
            return BurstResult(
                ok=False,
                exit_code=None,
                captured_output="",
                duration_seconds=time.monotonic() - start,
                error=f"Failed to launch agent window: {launch_proc.stderr}",
            )

        # Poll durable completion even if an interactive tmux shell stays alive.
        deadline_start = time.monotonic() + startup_timeout
        while time.monotonic() < deadline_start:
            failure = launch_failure(sandbox_cmd)
            if failure is not None:
                status, detail = failure
                tmux_kill_session(session_name)
                return BurstResult(ok=False, exit_code=status, captured_output=detail,
                                   duration_seconds=time.monotonic() - start,
                                   error="Burst Git preflight/bootstrap failed")
            if (provider_clock.read(execution_record, 'started') if require_turn_completion else start_file.exists()):
                break
            if tmux_pane_is_dead(pane_id):
                # The recorder may be finishing its atomic status publication.
                failure = launch_failure(sandbox_cmd)
                directory = report_directory(sandbox_cmd)
                stderr_file = directory / "bootstrap.stderr" if directory else None
                detail = (stderr_file.read_bytes().decode("utf-8", errors="replace")
                          if stderr_file and stderr_file.exists() else "")
                return BurstResult(ok=False, exit_code=failure[0] if failure else None,
                                   captured_output=failure[1] if failure else detail,
                                   duration_seconds=time.monotonic() - start,
                                   error="Agent pane died before startup")
            time.sleep(0.5)
        if not (provider_clock.read(execution_record, 'started') if require_turn_completion else start_file.exists()):
            failure = launch_failure(sandbox_cmd)
            directory = report_directory(sandbox_cmd)
            stderr_file = directory / "bootstrap.stderr" if directory else None
            detail = (stderr_file.read_bytes().decode("utf-8", errors="replace")
                      if stderr_file and stderr_file.exists() else "")
            tmux_kill_session(session_name)
            return BurstResult(
                ok=False,
                exit_code=failure[0] if failure else None,
                captured_output=failure[1] if failure else detail,
                duration_seconds=time.monotonic() - start,
                error=f"Agent failed to create startup marker before timeout ({startup_timeout}s)",
            )

        # New worker submissions require an actual CLI exit and completed turn.
        # A worker-written marker alone cannot freeze source or prove resumability.
        done_seen_at: Optional[float] = None
        completed_via_done = False
        timing = None
        while True:
            capture_session()
            if require_turn_completion:
                timing = provider_clock.read(execution_record, 'finished')
                if provider_clock.read(execution_record, 'exited') is not None:
                    break
            elif exit_file.exists():
                break
            if not require_turn_completion and done_file is not None and done_file.exists():
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
                if (provider_clock.read(execution_record, 'exited') if require_turn_completion else exit_file.exists()):
                    break
                if not require_turn_completion and done_file is not None and done_file.exists():
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
                if _has_turn_completed(tail_text):
                    _wait_for_process_exit(exit_file, pane_id, tmux_cmd)
                    break
                if tmux_pane_is_dead(pane_id):
                    break
                time.sleep(1)
            if role != 'engineering':
                tmux_kill_session(session_name)

    # Read result
    if require_turn_completion:
        timing = provider_clock.read(execution_record, 'finished')
        exit_code_text = str(provider_clock.read(execution_record, 'exited')['returncode'])
    elif role == 'engineering':
        exit_code_text = (_engineering_read(exit_file, optional=True) or '').strip() or '1'
    else:
        exit_code_text = exit_file.read_text().strip() if exit_file.exists() else ("0" if completed_via_done else "1")
    try:
        exit_code = int(exit_code_text)
    except ValueError:
        exit_code = 1

    time.sleep(0.5)
    output = ((_engineering_read(output_log, optional=True) or '') if role == 'engineering' else
              output_log.read_text(errors="replace") if output_log.exists() else "")

    if role != "engineering":
        tmux_kill_session(session_name)
    tool_host_error = _unfinished_tool_host_failure(output, done_file)
    effective_ok = exit_code == 0 and not tool_host_error

    # Parse usage from Codex JSON output (turn.completed event)
    usage = None
    for line in output.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
            if (
                isinstance(rec, dict)
                and rec.get("type") == "turn.completed"
                and isinstance(rec.get("usage"), dict)
            ):
                usage = rec["usage"]
                usage["provider"] = "codex"
                usage["model"] = config.model or "codex"
        except (json.JSONDecodeError, ValueError):
            pass

    thread_id = _extract_thread_id(output)
    if require_turn_completion:
        capture_session()
        thread_id = recorded_thread
        effective_ok = exit_code == 0 and bool(timing and timing['disposition'] == 'completed' and thread_id)
        tool_host_error = ''
        if not effective_ok:
            effective_ok = False
            tool_host_error = ("Worker execution allowance exhausted; retain available files for host checking"
                              if timing and timing['disposition'] == 'expired' else
                              "Worker turn did not complete with a resumable provider conversation; retain its files")
    reset_context = exit_code != 0 and (
        _detected_context_overflow(output)
        or _detected_compaction_bad_request(output)
        or _detected_policy_refusal(output)
    )
    if not require_turn_completion and reset_context and _persistent_context_scope(role, session_scope) and not ab_injected_fresh:
        # Overflow, rejected remote compaction, or a content-policy refusal
        # makes this history unusable.
        # The failed burst still returns its error; a bounded retry can start
        # fresh without repeatedly appending prompts to the rejected history.
        _clear_persisted_thread_id(work_dir, role, session_scope, config)
    elif not require_turn_completion and thread_id and _persistent_context_scope(role, session_scope) and not ab_injected_fresh:
        # Skip storing for a treatment-injected fresh burst: its throwaway
        # thread must not overwrite the persisted control session.
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
            ok=effective_ok,
            reason=tool_host_error,
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
            **(dict(engineering=True) if role == 'engineering' else {}),
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
        if thread_id and burst_home is not None:
            from trellis.codex_timing import find_rollout_path
            raw_transcript = find_rollout_path(
                thread_id, sessions_root=Path(burst_home) / ".codex" / "sessions"
            )
            if raw_transcript is not None:
                # Original logical path remains stable if terminal retention
                # archives it later; Trellis history readers resolve that form.
                # Native CLI resume still requires an explicitly restored raw file.
                call_meta["raw_transcript_path"] = str(raw_transcript)
        if role == 'engineering':
            _engineering_text(artifact_dir / 'call.json', json.dumps(call_meta, indent=2) + '\n')
        else:
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
    burst_error = tool_host_error
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
        else:
            # A non-zero exit must never surface as an empty error string:
            # callers that only see `response.error` (Phase 0's bridge, the
            # kernel's transport classification) would otherwise report
            # "agent transport failed: " with nothing to act on. Quote the
            # burst script's own stderr, which is where a pre-agent failure
            # (EROFS on the log redirect, a missing prompt file) lands.
            burst_error = f"codex exited {exit_code}"
            provider_errors = _codex_error_messages(output)
            if provider_errors:
                burst_error += f"; {provider_errors[-1][-2000:]}"
            if not output.strip():
                burst_error += " before producing any output"
            stderr_path = log_dir / f"{prefix}-burst.stderr"
            if role == 'engineering':
                stderr_tail = (_engineering_read(stderr_path, optional=True) or '').strip()[-2000:]
            else:
                try:
                    stderr_tail = stderr_path.read_text(errors="replace").strip()[-2000:]
                except OSError:
                    stderr_tail = ""
            if stderr_tail:
                burst_error += f"; burst stderr: {stderr_tail}"

    return BurstResult(
        ok=effective_ok,
        exit_code=exit_code,
        captured_output=output,
        duration_seconds=duration,
        usage=usage,
        error=burst_error,
        provider_session_id=thread_id,
        provider_turn_completed=effective_ok and (require_turn_completion or _has_turn_completed(output)),
        provider_execution=(provider_clock.read(clock_root, 'finished') if require_turn_completion else timing),
    )
