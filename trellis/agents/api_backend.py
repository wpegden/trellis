"""Direct Chat Completions agent for DeepSeek and Kimi, using only the stdlib.

HTTP runs in the supervisor. Every model-requested operation runs through the
same role-specific bwrap and Git admission used by the CLI backends. Worker
and review conversations persist in host-owned checkpoints, including reasoning.
"""
from __future__ import annotations

import argparse
import json
import io
import itertools
import math
import os
from pathlib import Path
import re
import shlex
import signal
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

from trellis.adapters import BurstResult, ProviderConfig
from trellis.chat_history import ensure_chat_file_link
from trellis.config import SandboxConfig
from trellis.git_trust import launch_failure, supervise_git_launch
from trellis.host_runtime import worker_elan_home, worker_path_env
from trellis.sandbox import wrap_command

PROVIDERS = {
    "deepseek": ("https://api.deepseek.com", "deepseek-v4-pro", "DEEPSEEK_API_KEY"),
    "kimi": ("https://api.moonshot.ai/v1", "kimi-k3", "KIMI_API_KEY"),
}
SYSTEM_PROMPT = """You are an autonomous Trellis coding and mathematical reasoning agent.
Complete the task in the user prompt, respecting its role and file restrictions.
Use the shell tool to inspect files, edit them, and run the provided checkers.
Each shell call starts in the task directory with a fresh shell. Use relative
paths or explicit cd commands. Long compiler/checker calls may take minutes;
set timeout_seconds appropriately. Read AGENTS.md if present in the task repo.
Write all requested artifacts to disk; describing them in chat does not create
them. Never claim a check passed unless you ran it and inspected its result.
When the task is finished, return a concise final response.
"""
SHELL_TOOL = {
    "type": "function",
    "function": {
        "name": "shell",
        "description": "Run a bash command in the task's role-specific sandbox. Read/edit files and run checkers here. Each call starts a fresh shell in the task directory.",
        "parameters": {
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_seconds": {"type": "number", "description": "Optional timeout, capped by the configured tool timeout and remaining burst time."},
            },
            "required": ["command"],
            "additionalProperties": False,
        },
    },
}


class BackendError(RuntimeError):
    pass


class _Parser(argparse.ArgumentParser):
    def error(self, message):
        raise BackendError(f"API backend options: {message}")


def options(config: ProviderConfig):
    parser = _Parser(add_help=False, allow_abbrev=False)
    parser.add_argument("--base-url")
    parser.add_argument("--max-turns", type=int)
    parser.add_argument("--max-tokens", type=int, default=32768)
    parser.add_argument("--request-timeout", type=float, default=300)
    parser.add_argument("--tool-timeout", type=float, default=7200)
    parser.add_argument("--max-output-chars", type=int, default=24000)
    parser.add_argument("--context-tokens", type=int, default=1000000 if (config.model or PROVIDERS[config.provider][1]) in {"deepseek-v4-pro", "deepseek-flash", "kimi-k3"} else 256000)
    parser.add_argument("--thinking", choices=("enabled", "disabled"))
    args = parser.parse_args(config.extra_args)
    for name in ("max_turns", "max_tokens", "request_timeout", "tool_timeout", "max_output_chars", "context_tokens"):
        value = getattr(args, name)
        if name == "max_turns" and value is None:
            continue
        if not math.isfinite(value) or value <= 0:
            raise BackendError(f"--{name.replace('_', '-')} must be finite and positive")
    if args.context_tokens <= args.max_tokens * 2:
        raise BackendError("--context-tokens must exceed twice --max-tokens")
    return args


def credentials(provider: str) -> tuple[str, str]:
    """Read literal KEY=value lines; never execute or source the credentials file.

    Environment wins. Only this provider's key and URL are loaded; keys never
    enter argv, generated scripts, or the shell tool's environment.
    """
    default_url, _, key_name = PROVIDERS[provider]
    url_name = provider.upper() + "_BASE_URL"
    values = {}
    path = Path(os.environ.get("TRELLIS_PROVIDERS_KEYS_FILE", "~/.config/trellis-providers/keys.env")).expanduser()
    if path.is_file():
        for line in path.read_text(encoding="utf-8").splitlines():
            # Ignore unrelated shell configuration, but fail on malformed
            # assignments for the selected provider without exposing the line.
            if not re.match(rf"^\s*(?:export\s+)?(?:{key_name}|{url_name})\s*=", line):
                continue
            try:
                parts = shlex.split(line, comments=True)
            except ValueError:
                raise BackendError(f"Malformed {key_name}/{url_name} assignment in {path}") from None
            if parts and parts[0] == "export":
                parts = parts[1:]
            if len(parts) != 1 or "=" not in parts[0]:
                raise BackendError(f"Expected literal KEY=value assignment in {path}")
            name, value = parts[0].split("=", 1)
            values[name] = value
    key = os.environ.get(key_name) or values.get(key_name)
    if not key:
        raise BackendError(f"Missing {key_name}; set it in the environment or {path}")
    return key, os.environ.get(url_name) or values.get(url_name) or default_url


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        # Do not forward credentials to a redirected host.
        return None


def _endpoint(base_url: str) -> str:
    parsed = urllib.parse.urlsplit(base_url)
    if (parsed.scheme != "https" and not
            (parsed.scheme == "http" and parsed.hostname in {"localhost", "127.0.0.1", "::1"})):
        raise BackendError("API base URL must use HTTPS (HTTP is allowed only for local tests)")
    if not parsed.hostname or parsed.username or parsed.password or parsed.query or parsed.fragment:
        raise BackendError("Invalid API base URL")
    return base_url.rstrip("/") + "/chat/completions"


def _completion(url: str, key: str, payload: dict, *, timeout: float, deadline: float) -> dict:
    data = json.dumps(payload).encode("utf-8")
    for attempt in range(3):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise BackendError("API burst timeout exceeded")
        request = urllib.request.Request(url, data=data, headers={
            "Authorization": "Bearer " + key, "Content-Type": "application/json",
        })
        try:
            with urllib.request.build_opener(_NoRedirect).open(request, timeout=min(timeout, remaining)) as response:
                result = json.load(response)
            if not isinstance(result, dict):
                raise BackendError("Provider returned a non-object response")
            return result
        except urllib.error.HTTPError as exc:
            detail = exc.read(8192).decode("utf-8", errors="replace").replace(key, "[REDACTED]")
            # Map provider-specific balance errors to the supervisor's existing
            # budget pause signal; these cannot be fixed by retries.
            from trellis.provider_budget import budget_exhaustion_detail
            exhausted = (exc.code == 402 or budget_exhaustion_detail(error=detail) or
                         any(word in detail.lower() for word in ("insufficient balance", "insufficient_balance", "balance is not enough")))
            if exhausted:
                raise BackendError(f"insufficient_quota: HTTP {exc.code}: {detail}") from None
            if (exc.code == 429 or 500 <= exc.code < 600) and attempt < 2:
                delay = 2 ** (attempt + 1)
                if time.monotonic() + delay < deadline:
                    time.sleep(delay)
                    continue
            raise BackendError(f"Provider HTTP {exc.code}: {detail}") from None
        except (urllib.error.URLError, TimeoutError) as exc:
            # Ambiguous network failures may have incurred usage. Don't replay.
            raise BackendError(f"Provider connection failed: {exc}") from None
    raise AssertionError("unreachable")


def _kill_group(proc: subprocess.Popen) -> None:
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    proc.wait()


def _shell(command: str, *, work_dir: Path, role: str, sandbox: SandboxConfig | None,
           burst_home: Path | None, timeout: float, max_output: int) -> dict:
    env = dict(os.environ)
    for _, _, name in PROVIDERS.values():
        env.pop(name, None)
    env["PATH"] = worker_path_env(burst_home, isabelle=(work_dir / "ROOT").exists())
    env["ELAN_HOME"] = str(worker_elan_home())
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    if burst_home is not None:
        env["HOME"] = str(burst_home)
    wrapped = wrap_command(["/bin/bash", "--noprofile", "--norc", "-c", command],
                           sandbox=sandbox, work_dir=work_dir, burst_home=burst_home,
                           role=role, git_preflight_path=env["PATH"])
    # Spool output rather than retaining unbounded compiler output in memory.
    with tempfile.TemporaryFile() as output:
        proc = subprocess.Popen(supervise_git_launch(wrapped), cwd=work_dir, env=env,
                                stdin=subprocess.DEVNULL, stdout=output,
                                stderr=subprocess.STDOUT, start_new_session=True)
        timed_out = False
        try:
            proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
        finally:
            # Also remove background descendants after a successful shell exit.
            _kill_group(proc)
        size = output.tell()
        output.seek(0)
        if size > max_output:
            half = max_output // 2
            head = output.read(half)
            output.seek(-half, os.SEEK_END)
            raw = head + b"\n[output truncated]\n" + output.read(half)
        else:
            raw = output.read(max_output)
    failure = launch_failure(wrapped)
    if failure:
        raise BackendError(f"Burst Git preflight/bootstrap failed: {failure[1]}")
    return {"exit_code": proc.returncode, "timed_out": timed_out,
            "output": raw.decode("utf-8", errors="replace")}


def _tool_result(call: dict, *, work_dir: Path, role: str, sandbox, burst_home,
                 args, deadline: float) -> dict:
    try:
        function = call["function"]
        if function["name"] != "shell":
            raise ValueError(f"Unknown tool: {function['name']}")
        params = json.loads(function["arguments"])
        if not isinstance(params, dict) or set(params) - {"command", "timeout_seconds"}:
            raise ValueError("Expected command and optional timeout_seconds")
        command = params.get("command")
        if not isinstance(command, str) or not command.strip():
            raise ValueError("command must be a nonempty string")
        timeout = params.get("timeout_seconds", args.tool_timeout)
        if type(timeout) not in (int, float) or not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("timeout_seconds must be finite and positive")
    except (ValueError, KeyError, TypeError) as exc:
        return {"error": f"Invalid tool call: {exc}"}
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise BackendError("API burst timeout exceeded")
    return _shell(command, work_dir=work_dir, role=role, sandbox=sandbox,
                  burst_home=burst_home, timeout=min(timeout, args.tool_timeout, remaining),
                  max_output=args.max_output_chars)


def _add_usage(total: dict, raw: dict) -> None:
    prompt = int(raw.get("prompt_tokens", 0) or 0)
    completion = int(raw.get("completion_tokens", 0) or 0)
    cached = int(raw.get("prompt_cache_hit_tokens", 0) or
                 (raw.get("prompt_tokens_details") or {}).get("cached_tokens", 0) or 0)
    reasoning = int((raw.get("completion_tokens_details") or {}).get("reasoning_tokens", 0) or 0)
    for name, amount in (("input_tokens", prompt), ("output_tokens", completion),
                         ("cached_input_tokens", cached), ("reasoning_tokens", reasoning),
                         ("total_tokens", int(raw.get("total_tokens", prompt + completion) or 0))):
        total[name] += amount


def role_instructions(key):
    # Reuse the maintained role standards; CLI-specific interaction instructions
    # do not describe this harness, whose only tool is a synchronous shell.
    stem = re.sub(r"[^A-Za-z0-9_.-]+", "_", str(key or "")).strip("._-")[:80]
    path = Path(__file__).with_name('codex_base_instructions') / (stem + '.md')
    if not stem or not path.is_file():
        return SYSTEM_PROMPT
    guidance = path.read_text().split('# Working with the user', 1)[0]
    guidance = guidance.replace('You are Codex, a coding agent based on GPT-5. ', '')
    guidance = '\n'.join(line for line in guidance.splitlines()
        if not any(tool in line for tool in ('multi_tool_use.parallel', 'exec_command', 'apply_patch')))
    return guidance + '\n' + SYSTEM_PROMPT + (
        '\nTool interface: shell is the only available tool. Edit files with shell commands or Python. '
        'Calls wait until exit or timeout; there are no exec_command/write_stdin sessions. '
        'For long Lean/Rust checkers set timeout_seconds up to the remaining burst allowance. '
        'The host persists and compacts your conversation automatically.\n')


def _run(config: ProviderConfig, prompt: str, *, session_name: str, work_dir: Path,
        role: str = "worker", session_scope: str = "", startup_timeout: float = 3600,
        burst_timeout: float = 7200, log_dir: Path | None = None,
        artifact_prefix: str | None = None, done_file: Path | None = None,
        sandbox: SandboxConfig | None = None, burst_home: Path | None = None,
        fresh: bool = False, require_turn_completion: bool = False,
        exact_resume_thread_id: str = "", session_record: Path | None = None,
        execution_record: Path | None = None, base_instructions_key: str | None = None, _events_only: bool = False) -> BurstResult:
    start = time.monotonic()
    deadline = start + burst_timeout
    work_dir = work_dir.resolve()
    log_dir = log_dir or work_dir / ".trellis/logs/bursts"
    prefix = re.sub(r"[^A-Za-z0-9_.-]+", "_", artifact_prefix or role)[:80]
    transcript = ensure_chat_file_link(work_dir, log_dir=log_dir, artifact_prefix=prefix,
                                      role=role, log_filename=f"{prefix}-output.log", canonical_name="output.log")
    prompt_file = ensure_chat_file_link(work_dir, log_dir=log_dir, artifact_prefix=prefix,
                                       role=role, log_filename=f"{prefix}-prompt.txt", canonical_name="prompt.txt")
    session_id = str(uuid.uuid4())
    model = config.model or PROVIDERS[config.provider][1]
    usage = dict(provider=config.provider, model=model, input_tokens=0, output_tokens=0,
                 cached_input_tokens=0, reasoning_tokens=0, total_tokens=0, requests=0)
    error, key = "", ""
    completed, ok = False, False
    records = []
    # A pre-existing artifact must not turn an unrelated failure into success.
    initial_done = done_file.stat().st_mtime_ns if done_file and done_file.exists() else None

    from contextlib import ExitStack
    from dataclasses import replace
    from trellis.agents.api_sessions import conversation
    with ExitStack() as stack:
        log = stack.enter_context(io.StringIO() if _events_only else transcript.open("w", encoding="utf-8"))
        if not _events_only:
            transcript.chmod(0o600)

        def redact(text):
            return text.replace(key, "[REDACTED]") if key else text

        def emit(kind, **fields):
            line = redact(json.dumps({"type": kind, **fields}, ensure_ascii=False))
            records.append(line)
            log.write(line + "\n")
            log.flush()
            if _events_only:
                print(line, flush=True)

        try:
            args = options(config)
            key, base_url = credentials(config.provider)
            project_config = work_dir / 'trellis.config.json'
            project_url = ''
            if project_config.is_file():
                project_url = (json.loads(project_config.read_text()).get('api_providers', {})
                               .get(config.provider, {}).get('base_url', ''))
            url = _endpoint(args.base_url or project_url or base_url)
            prompt_file.write_text(redact(prompt), encoding="utf-8")
            prompt_file.chmod(0o600)
            effective = replace(config, model=model, effort=config.effort or "max")
            state, save = stack.enter_context(conversation(config=effective, work_dir=work_dir,
                home=burst_home, role=role, scope=session_scope,
                fresh=fresh or (require_turn_completion and not exact_resume_thread_id),
                exact_thread=exact_resume_thread_id, endpoint=url))
            session_id = state['binding']['thread_id']
            messages = state['messages']
            if not messages:
                messages.append({"role": "system", "content": role_instructions(base_instructions_key or role)})
            messages.append({"role": "user", "content": prompt})
            save()
            if require_turn_completion:
                from trellis.agent_wrapper.continuation import record_session
                record_session(session_record, state['binding'])
            emit("thread.started", thread_id=session_id, provider=config.provider, model=model)
            emit("turn.started")
            # Long proof submissions routinely need hundreds of operations.
            # The owned execution clock is the default limit; retain an
            # optional request cap for callers that explicitly want one.
            for turn in itertools.count():
                if args.max_turns is not None and turn >= args.max_turns:
                    raise BackendError(f"API burst reached --max-turns={args.max_turns}")
                if time.monotonic() >= deadline:
                    raise BackendError("API burst timeout exceeded")
                payload = dict(model=model, messages=messages, tools=[SHELL_TOOL],
                               max_tokens=args.max_tokens, stream=False)
                if config.effort or model in {"deepseek-v4-pro", "deepseek-flash", "kimi-k3"}:
                    payload["reasoning_effort"] = effective.effort
                if args.thinking:
                    payload["thinking"] = {"type": args.thinking}
                request_deadline = deadline  # Model reasoning is execution, not launcher startup.
                def complete(request):
                    result = _completion(url, key, request, timeout=args.request_timeout, deadline=request_deadline)
                    raw_usage = result.get("usage") or {}
                    if not isinstance(raw_usage, dict):
                        raise BackendError("Provider returned invalid usage")
                    _add_usage(usage, raw_usage)
                    usage["requests"] += 1
                    emit("usage", usage=raw_usage, model=result.get("model", model))
                    return result

                from trellis.agents.api_context import compact, size
                def compact_context():
                    archive = compact(state, save, complete=complete, payload=payload, context_tokens=args.context_tokens)
                    emit("context.compacted", archive=archive)

                compacted = size(messages) + args.max_tokens > args.context_tokens * 0.7
                if compacted:
                    compact_context()
                try:
                    response = complete(payload)
                except BackendError as exc:
                    detail = str(exc).lower()
                    overflow = 'http 400' in detail and any(term in detail for term in
                        ('context_length_exceeded', 'maximum context length', 'context window', 'context length'))
                    if not overflow or compacted:
                        raise
                    compact_context()
                    response = complete(payload)
                choices = response.get("choices")
                if not isinstance(choices, list) or not choices:
                    raise BackendError("Provider returned no completion choices")
                choice = choices[0]
                if not isinstance(choice, dict):
                    raise BackendError("Provider returned an invalid completion choice")
                message = choice.get("message")
                if not isinstance(message, dict) or message.get("role") != "assistant":
                    raise BackendError("Provider returned an invalid assistant message")
                if choice.get("finish_reason") not in {"stop", "tool_calls"}:
                    raise BackendError(f"Provider completion stopped: {choice.get('finish_reason')!r}; increase --max-tokens if truncated")
                calls = message.get("tool_calls") or []
                if not isinstance(calls, list) or any(not isinstance(c, dict) or not isinstance(c.get("id"), str) for c in calls):
                    raise BackendError("Provider returned invalid tool call IDs")
                # Preserve reasoning_content exactly, including across tool calls.
                assistant = {name: message[name] for name in ("role", "content", "reasoning_content", "tool_calls") if name in message}
                messages.append(assistant)
                save()
                calls = message.get("tool_calls") or []
                if message.get("content"):
                    emit("item.completed", item={"type": "agent_message", "text": message["content"]})
                if not calls:
                    if not message.get("content"):
                        raise BackendError("Provider returned an empty final answer")
                    completed, ok = True, True
                    emit("turn.completed", usage=usage)
                    break
                for call in calls:
                    try:
                        command = json.loads(call["function"]["arguments"]).get("command", "")
                    except (ValueError, KeyError, TypeError, AttributeError):
                        command = ""
                    item = {"id": call["id"], "type": "command_execution", "command": command}
                    emit("item.started", item={**item, "tool_call": call})
                    result = _tool_result(call, work_dir=work_dir, role=role, sandbox=sandbox,
                                          burst_home=burst_home, args=args, deadline=deadline)
                    content = redact(json.dumps(result, ensure_ascii=False))
                    messages.append({"role": "tool", "tool_call_id": call["id"], "content": content})
                    save()
                    emit("item.completed", item={**item, "exit_code": result.get("exit_code"),
                                                 "aggregated_output": content})
                    if not require_turn_completion and done_file and done_file.exists() and done_file.stat().st_mtime_ns != initial_done:
                        ok = True
                        break
                if ok:
                    break
        except (RuntimeError, OSError, ValueError, TypeError, KeyError) as exc:
            error = redact(str(exc))
            emit("turn.failed", error={"message": error})

    duration = time.monotonic() - start
    from trellis.agents.tmux_backend import append_cost_ledger
    if not _events_only:
        append_cost_ledger(work_dir, provider=config.provider, role=role, scope=session_scope,
                       model=model, usage=usage, cost_usd=None, duration_seconds=duration,
                       attempts=1, ok=ok, reason=error or "completed", session_id=session_id,
                       message_count=usage["requests"], extra={"backend": "api"})
    return BurstResult(ok=ok, exit_code=0 if ok else 1, captured_output="\n".join(records),
                       duration_seconds=duration, usage=usage, error=error,
                       transcript_path=transcript, provider_session_id=session_id,
                       provider_turn_completed=completed)


def run(config: ProviderConfig, prompt: str, **kwargs) -> BurstResult:
    if kwargs.get('exact_resume_thread_id') and not kwargs.get('require_turn_completion'):
        raise ValueError('exact worker continuation requires the submission lifecycle')
    if kwargs.get('require_turn_completion'):
        from trellis.agents.api_execution import run_monitored
        return run_monitored(config, prompt, **kwargs)
    return _run(config, prompt, **kwargs)


if __name__ == '__main__':
    from trellis.agents.api_execution import child_main
    raise SystemExit(child_main())
