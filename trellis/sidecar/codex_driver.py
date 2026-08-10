"""Codex-CLI attempt backend for grunts.

WHY THIS IS A SEPARATE DRIVER RATHER THAN A TRANSPORT SWAP

The arm this replaced was not a thin model client: it ran the sidecar's
OWN agentic loop — its own tool schemas (`search_tablet`,
`search_mathlib`, `get_goals`, `read_file`), its own compaction, its own
budget accounting — against an OpenAI-compatible `chat/completions`
endpoint. `codex exec` is itself an agent and exposes no equivalent raw
chat surface, so there was no place to slot it in underneath that loop's
chat call. The backend had to replace the loop, not the transport — and
now that the HTTP arm is retired, this file IS the loop.

It swaps in at exactly one seam. `attempt.py` runs

    refresh workspace -> giant check -> compile loop -> DRIVER -> prevalidate -> publish

and only the DRIVER step changes. Everything downstream is untouched
because this returns the same `AttemptResult`, and the field that matters
downstream is `proof_body`: `prevalidate_success` re-validates the body
itself, so the agent never gets to declare its own success.

TWO INVARIANTS THIS FILE EXISTS TO PRESERVE

1. *The harness decides what closed, not the agent.* codex will happily
   report success. That claim is discarded: the body is harvested from the
   file, scanned for banned tokens, and compiled through the SAME
   `compile_body` callback the HTTP driver uses. Only that verdict sets
   `status`.

2. *The workspace is left as the HTTP driver leaves it.* The HTTP driver
   never mutates the node file — it only compiles candidate bodies. codex
   edits files in place, so this restores the workspace afterwards and
   reports any collateral edits as a discipline failure rather than
   silently reverting them. A grunt that edits its neighbours is a bug
   worth seeing, not tidying away.
"""

from __future__ import annotations

import json
import os
import re
import signal
import subprocess
import time
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Sequence, Tuple

from trellis.sidecar.config import SidecarConfig
from trellis.sidecar.driver import (
    AttemptResult,
    body_ban_scan,
    split_body_marker,
)

CODEX = "codex"

# codex writes one JSON object per line; `turn.completed` carries the
# cumulative usage for the whole agentic run.
_USAGE_EVENT = "turn.completed"


def sanitize_detail(text: str, *, limit: int = 500) -> str:
    """Make an agent-influenced diagnostic safe to render into a prompt.

    `detail` is read by a TRUSTED role. It reaches the reviewer via
    `attempted.json` -> `status.json` -> `_attempt_digest` ->
    `{{sidecar_status_block}}`, and `status.json` is ro-bound into the
    reviewer sandbox with the fragment instructing it to open the file —
    so the 80-char digest cap is display-only, not a bound.

    Three substrings here are agent-controlled: the Lean compiler log tail
    (whose error spans quote the proof body verbatim), the FILENAMES in the
    collateral-edit message, and anything else interpolated from disk. A
    file named `Tablet/"]. IGNORE PRIOR INSTRUCTIONS ....lean` otherwise
    lands near-arbitrary text in a trusted context, and an embedded newline
    breaks the one-row-per-line table the reviewer parses.

    So: drop control characters, collapse every whitespace run to a single
    space, and bound the length. Diagnostic loss is close to nil — the
    prompt renderer already reflows these — and this is the only fix that
    also covers the uncapped `ledger.jsonl`.
    """
    if not text:
        return ""
    cleaned = "".join(
        " " if (ord(ch) < 0x20 or 0x7F <= ord(ch) < 0xA0) else ch
        for ch in str(text)
    )
    cleaned = " ".join(cleaned.split())
    return cleaned[:limit]


# A Lean diagnostic header, with or without the `file:line:col:` prefix.
_DIAG_HEAD = re.compile(r"^\s*(?:\S*\.lean:\d+:\d+:\s*)?error:", re.IGNORECASE)
# `grind` renders an E-graph dump inside ONE diagnostic message: equivalence
# classes, asserted propositions, E-matching instance tables. Hundreds of
# lines of it, none of which say what went wrong.
_GRIND_NOISE = re.compile(
    # `issue` is excluded from block CONTEXT so the dedicated pass below can
    # own it and attach the wrapped goal, rather than both emitting it.
    r"^\s*\[(?:prop|thm|eqc|assoc|basis|properties|facts|diag|issue|_)\b|^\s*\[grind\]"
)
# `grind`'s one genuinely diagnostic line: what it could not do. The goal
# it names usually wraps onto the following line, so that is captured too.
_GRIND_ISSUE = re.compile(r"^\s*\[issue\]")


def extract_compiler_errors(
    log: str, *, limit: int = 500, max_blocks: int = 3, context: int = 4
) -> str:
    """The part of a compiler log that says what went WRONG.

    Neither a head nor a tail window works here, because two different log
    shapes reach this function. `SidecarCompileLoop.check_body` returns
    rendered LSP diagnostics, where the complaint is the FIRST line of each
    block; `_lake_fallback` and `confirm` return `lake build` output, where
    the errors are at the END after the build chatter. A fixed window is
    right for one and wrong for the other.

    The observed failure: a `grind` timeout renders as a single enormous
    diagnostic, so `log[-600:]` captured its E-matching instance tables and
    the stored detail read `[prop] ... [eqc] ...` with no error line in it —
    undiagnosable. (`perfect` run, node `twisty`, 2026-07-31.)

    So: find the `error:` lines wherever they sit, keep a little context
    under each, and drop `grind`'s E-graph tables while KEEPING its
    `[issue]` line, which names the goal it failed on. Falls back to the
    tail when nothing matches, which is the old behaviour and the right
    guess for an unrecognised shape.
    """
    if not log:
        return ""
    lines = log.splitlines()
    picked: List[str] = []
    blocks = 0
    i = 0
    while i < len(lines) and blocks < max_blocks:
        if _DIAG_HEAD.search(lines[i]):
            blocks += 1
            picked.append(lines[i].strip())
            kept = 0
            j = i + 1
            while j < len(lines) and kept < context:
                nxt = lines[j]
                if _DIAG_HEAD.search(nxt):
                    break
                # A grind marker opens the E-graph dump: everything below it
                # belongs to the dump, so STOP rather than skip — skipping
                # walks on and picks dump interior lines as if they were
                # goal context.
                if _GRIND_NOISE.search(nxt):
                    break
                if nxt.strip():
                    picked.append(nxt.strip())
                    kept += 1
                j += 1
            i = j
            continue
        i += 1
    # grind's `[issue]` names the actual obstruction — worth more than the
    # generic "`grind` failed" header it hides under. The goal it names
    # wraps onto the next line, so take that with it.
    for idx, line in enumerate(lines):
        if not _GRIND_ISSUE.search(line):
            continue
        issue = line.strip()
        if idx + 1 < len(lines) and lines[idx + 1].strip():
            nxt = lines[idx + 1]
            if not _GRIND_NOISE.search(nxt) and not _DIAG_HEAD.search(nxt):
                issue = f"{issue} {nxt.strip()}"
        if issue not in picked:
            picked.append(issue)
        break
    if not picked:
        return sanitize_detail(log[-600:], limit=limit)
    return sanitize_detail(" | ".join(picked), limit=limit)


def _tablet_path(repo: Path, node: str) -> Path:
    return Path(repo) / "Tablet" / f"{node}.lean"


def _git(repo: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
    )


def _dirty_paths(repo: Path) -> List[str]:
    proc = _git(repo, "status", "--porcelain")
    out = []
    for line in (proc.stdout or "").splitlines():
        entry = line[3:].strip()
        if entry:
            out.append(entry)
    return out


def parse_codex_stream(path: Path) -> Dict[str, Any]:
    """Usage + coarse activity counts from a `codex exec --json` stream.

    Tolerant by construction: a wall-clock kill truncates the stream
    mid-line and never emits `turn.completed`, which is precisely the case
    we most need numbers for. Unparseable lines are skipped rather than
    failing the attempt.
    """
    usage: Dict[str, Any] = {}
    thread_id: Optional[str] = None
    last_message = ""
    agent_messages = 0
    command_calls = 0
    lines = 0
    try:
        text = path.read_text(errors="replace")
    except OSError:
        return {"usage": {}, "thread_id": None, "last_message": "",
                "agent_messages": 0, "command_calls": 0,
                "stream_lines": 0}
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        lines += 1
        try:
            event = json.loads(line)
        except Exception:
            continue
        etype = event.get("type")
        if etype == "thread.started" and event.get("thread_id"):
            thread_id = str(event["thread_id"])
        if etype == _USAGE_EVENT and isinstance(event.get("usage"), dict):
            usage = event["usage"]
        item = event.get("item") or {}
        itype = item.get("type") if isinstance(item, dict) else None
        if itype == "agent_message":
            agent_messages += 1
            text = item.get("text")
            if isinstance(text, str) and text.strip():
                last_message = text
        elif itype in ("command_execution", "local_shell_call"):
            command_calls += 1
    return {
        "usage": usage,
        "thread_id": thread_id,
        "last_message": last_message,
        "agent_messages": agent_messages,
        "command_calls": command_calls,
        "stream_lines": lines,
    }


def _find_usage(obj: Any) -> Dict[str, Any]:
    """Depth-first hunt for the innermost dict carrying input_tokens."""
    if isinstance(obj, dict):
        if "input_tokens" in obj and isinstance(obj.get("input_tokens"), int):
            return dict(obj)
        for value in obj.values():
            got = _find_usage(value)
            if got:
                return got
    elif isinstance(obj, list):
        for value in obj:
            got = _find_usage(value)
            if got:
                return got
    return {}


def rollout_usage_fallback(codex_home: Optional[Path], thread_id: Optional[str]) -> Dict[str, Any]:
    """Last usage snapshot from the session rollout file.

    A run killed at the wall never emits `turn.completed`, so the stream
    carries no usage — and those are the LONGEST, most expensive attempts,
    the ones whose cost most needs counting. Without this, wall-killed
    grunts report zero tokens and the quota they actually spent is
    invisible.
    """
    if not codex_home:
        return {}
    sessions = Path(codex_home) / "sessions"
    if not sessions.is_dir():
        return {}
    candidates = sorted(sessions.rglob("rollout-*.jsonl"), key=lambda p: p.stat().st_mtime)
    if thread_id:
        matched = [p for p in candidates if thread_id in p.name]
        if matched:
            candidates = matched
    if not candidates:
        return {}
    best: Dict[str, Any] = {}
    for raw in candidates[-1].read_text(errors="replace").splitlines():
        if "input_tokens" not in raw:
            continue
        try:
            parsed = json.loads(raw)
        except ValueError:
            continue
        found = _find_usage(parsed)
        if found:
            best = found
    return best


def build_codex_task(
    node: str,
    system_prompt: str,
    *,
    mathlib_rel: Optional[str] = None,
    wall_seconds: Optional[float] = None,
    banned_tokens: Optional[Sequence[str]] = None,
    loogle_enabled: bool = False,
    previous_errors: Optional[str] = None,
) -> str:
    """The sidecar's own system prompt, plus the file-editing contract and
    the retrieval surface.

    The mathematical content is `build_system_prompt_v2`'s, unchanged.
    What is added is the part a chat protocol carried structurally and a
    CLI agent has to be told: which file, which region, and that the
    frozen prefix is not yours to touch.

    The retrieval paragraph matters as much as the editing one. The prose
    proof is never carried in the prompt — it stays ON DISK, and the
    retired HTTP arm reached it through a `read_file` tool that allowed
    `Tablet/<Stem>.{lean,tex}` for ANY node plus mathlib source. Building
    this arm's prompt with the tool flags off (correct — those tools are
    unreachable from a CLI agent) therefore silently removed the .tex side
    as well: codex can open those files with its own shell, but nothing
    ever told it they exist. Naming them restores the same surface through
    the mechanism this agent actually has.
    """
    target = f"Tablet/{node}.lean"
    mathlib = mathlib_rel or ".lake/packages/mathlib/Mathlib"

    # The ban scan is a plain, comment-inclusive, word-boundary text scan
    # run AFTER the agent is gone, so a banned word in an explanatory
    # comment discards a compiling, correct proof with no chance to repair
    # it. The retired HTTP arm never had this problem: it scanned before
    # compiling and fed the rejection back for another turn. Naming them is
    # the cheap half of closing that gap — several of them (`partial`,
    # `constant`, `axiom`) are ordinary English a proof comment reaches for.
    tokens = list(banned_tokens or [])
    banned_paragraph = ""
    if tokens:
        banned_paragraph = (
            "These tokens must not appear anywhere in the body, INCLUDING "
            "inside comments — the check is a plain text scan run after you "
            "finish, and it discards the whole attempt: "
            + ", ".join(f"`{t}`" for t in tokens)
            + ". Several are ordinary English; avoid them in prose too, and "
            "do not name them even to say you are avoiding them.\n\n"
        )

    # `build_system_prompt_v2`'s persistence directive rides on the
    # `goals_enabled` flag, which this arm turns off — so the arm whose only
    # budget is a wall was the one that lost "keep working" and gained an
    # explicit "stop". Restate it here, with the actual budget.
    if wall_seconds:
        persistence = (
            f"You have about {int(wall_seconds // 60)} minutes. This is a long "
            "task: keep iterating until the body compiles or the time is gone. "
            "If a tactic fails, read the error, adjust, and build again.\n\n"
            # Observed give-up mode (`perfect`, node `twisty`): the agent hit a
            # `grind` heartbeat timeout, reported "the direct automation times
            # out", and closed the turn at 157s of a 900s budget saying it was
            # out of time. A tactic's budget is not yours; say so explicitly.
            "A tactic that times out or exhausts its heartbeats has spent ITS "
            "budget, not yours. That is one failed tactic, not the end of your "
            "time — drop it, try a different approach, and build again.\n\n"
            # Agents keep closing with "I couldn't complete a compiling proof
            # within the available iteration time" while hundreds of seconds
            # remain — `TwoFanMenger` said it with 659s left and the attempt
            # ended having used 360s of 900s. There is no iteration ceiling to
            # run out of, so name the only clock that exists.
            f"The {int(wall_seconds // 60)} minutes above is your ONLY budget. "
            "There is no separate limit on turns, iterations, or tool calls, "
            "so keep working until the body compiles or that time is gone.\n\n"
        )
    else:
        persistence = (
            "This is a long task: keep iterating until the body compiles.\n\n"
        )

    # Retry round: the previous body is still on disk and did not compile.
    # The compiler's own words are the most useful thing we can hand back.
    retry_paragraph = ""
    if previous_errors:
        retry_paragraph = (
            "This is a RETRY. The body currently in the file is your previous "
            "attempt, and it does NOT compile. Our check reported:\n\n"
            f"{previous_errors}\n\n"
            "Read the file, diagnose that error, and fix it. If your previous "
            "approach is a dead end, replace it rather than patching it.\n\n"
        )

    return (
        f"{system_prompt}\n\n"
        "---\n\n"
        "Your current working directory is the root of a Lean checkout, and "
        "every path below is relative to it.\n\n"
        f"Edit the file `{target}` in place.\n\n"
        f"Everything ABOVE the `-- BODY` line in {target} is frozen: do not "
        "change it, not even whitespace. Replace only the proof body BELOW "
        "that line.\n\n"
        # Two attempts in one hour rewrote the file without the marker line
        # and were discarded whole — `knottype` at 835k tokens,
        # `StaircaseCompleteVertices` at 319k. The instruction above says
        # what is frozen but never says the separator itself must survive,
        # which a whole-file rewrite silently drops.
        "The file must still contain the line `-- BODY`, exactly once, when "
        "you finish. It is the only thing separating the frozen part from "
        "your proof, and the attempt is discarded without it.\n\n"
        # NOT restored: an instruction to "leave your best attempt in the
        # file". It was added to stop agents reverting their work, and it
        # backfired measurably — give-up language rose 30% -> 70% and agents
        # began submitting stubs as the deliverable (`by classical aesop`,
        # "It does not compile"). It also broke the retry loop: the agent
        # preserves round 1's body as instructed, the driver scores that as
        # an unchanged body — an arm that was TERMINAL at the time, so
        # round 1's compile error was discarded. 18 failures, 17 of them on
        # round 2, were caused this way. Reverted rather than reworded: the
        # behaviour it targeted costs one wasted round, while this cost the
        # deliverable itself. (The unchanged arm has since been made
        # non-terminal harness-side; the give-up-language regression is why
        # this prompt instruction stays out regardless.)
        f"Change no file other than {target}.\n\n"
        "Read whatever helps, with ordinary shell tools:\n\n"
        f"* `Tablet/{node}.tex` — the paper's statement and prose proof of "
        "THIS node. Start here: it is the argument you are formalizing.\n"
        "* `Tablet/<OtherNode>.tex` and `Tablet/<OtherNode>.lean` for any "
        "other node — the .lean files of already-closed nodes carry the "
        "idioms this tablet uses, and the winning one often lives in a "
        "sibling this node never imports.\n"
        f"* `{mathlib}/**.lean` — mathlib source, for the exact form of a "
        "lemma you mean to apply.\n"
        "* `grep`/`rg` over those trees to find a name or an idiom.\n\n"
        "\nWork with:\n\n"
        f"* `lake build Tablet.{node}` to compile just this node. Use that "
        "target, never a bare `lake build`: the bare form rebuilds the whole "
        "tablet and can consume your entire time budget.\n"
        "* To see the goal state at a point, leave the tactic block "
        "unfinished there (or insert `trace_state`) and build — Lean prints "
        "the remaining goals in the error.\n"
        f"* `.trellis/scratch/` for any scratch file you want. Create files "
        "nowhere else in the tree: a new file anywhere else fails the "
        "attempt, even when the proof itself is correct.\n"
        + (
            "* `curl -s 'http://127.0.0.1:8088/json?q=<urlencoded query>'` to "
            "search mathlib by shape (loogle).\n"
            if loogle_enabled else ""
        )
        + "\n"
        + banned_paragraph
        + retry_paragraph
        + persistence
        + f"Verify with `lake build Tablet.{node}` before you finish. When the "
        "body compiles with no errors and no `sorry`, you are done.\n"
    )


def refresh_grunt_credential(codex_home: Path) -> bool:
    """Re-seed the grunt's ``auth.json`` from the operator copy the main
    run keeps valid, returning whether bytes were written.

    The grunt's ``CODEX_HOME`` is deliberately its own directory, so its
    ``auth.json`` is a distinct inode rather than a hardlink of the file
    ``$HOME/.codex``, the worker burst home, and the reviewer burst home
    all share. That isolation is worth keeping — a grunt credential
    failure must not be able to unauthenticate the formalization loop —
    but it also gives the copy its own refresh lineage, which is how it
    came to hold a dead token while the shared file was still fine.

    Refreshing at launch keeps that lineage short: each attempt starts
    from a token the main run is still transacting on, so the grunt
    rarely reaches expiry and rarely refreshes. That matters because
    several grunts share one ``CODEX_HOME``; concurrent refreshes race,
    and the loser is told its refresh token "was already used".

    Advisory by construction — a sync failure leaves the existing
    credential in place, which may well still authenticate.
    """
    master = Path.home() / ".codex" / "auth.json"
    target = Path(codex_home) / "auth.json"
    try:
        if target.resolve() == master.resolve():
            return False  # already the shared file; nothing to copy
        payload = master.read_bytes()
        if target.exists() and target.read_bytes() == payload:
            return False
        target.parent.mkdir(parents=True, exist_ok=True)
        # Same-directory temp + atomic replace: a grunt reading the file
        # concurrently sees either the old bytes or the new ones.
        tmp = target.with_name(f".{target.name}.sync-{os.getpid()}")
        tmp.write_bytes(payload)
        os.chmod(tmp, 0o600)
        os.replace(tmp, target)
        return True
    except OSError:
        return False


def _attempt_round(
    *,
    config: SidecarConfig,
    repo: Path,
    node: str,
    node_content: str,
    system_prompt: str,
    initial_body: str,
    compile_body_factory: Callable[[], Callable[[str], Any]],
    stream_path: Optional[Path] = None,
    codex_home: Optional[Path] = None,
    log: Callable[[str], None] = lambda _m: None,
    now: Callable[[], float] = time.monotonic,
    wall: Optional[float] = None,
    previous_errors: Optional[str] = None,
) -> AttemptResult:
    """ONE codex turn, scored. `run_attempt_codex` below loops over this.

    `initial_body` is this ROUND's starting body — the node's opening body
    on round 1, the previous round's failed body on a retry — so the
    "unchanged body" arm keeps meaning "this turn did nothing".
    """
    repo = Path(repo)
    node_file = _tablet_path(repo, node)
    original_prefix, _original_body = split_body_marker(node_content)
    wall = float(config.attempt_wall_seconds if wall is None else wall)

    started = now()
    result = AttemptResult(status="error", reasoning_effort=config.reasoning_effort or "")

    if not node_file.exists():
        result.detail = f"node file missing in workspace: {node_file}"
        result.wall_secs = now() - started
        return result

    # Baseline: anything already dirty is not this attempt's doing, and
    # must not be reported as a discipline violation.
    pre_dirty = set(_dirty_paths(repo))

    # Point at this workspace's actual mathlib checkout when it has one, so
    # the prompt never names a path that is not there.
    try:
        from trellis.sidecar.driver import mathlib_source_root

        root = mathlib_source_root(repo)
        mathlib_rel = str(root.relative_to(repo)) if root else None
    except Exception:
        mathlib_rel = None
    from trellis.sidecar.driver import BANNED_TOKENS

    task = build_codex_task(
        node,
        system_prompt,
        mathlib_rel=mathlib_rel,
        wall_seconds=wall,
        banned_tokens=BANNED_TOKENS,
        loogle_enabled=bool(config.loogle_enabled),
        previous_errors=previous_errors,
    )
    stream_path = Path(stream_path) if stream_path else (repo.parent / f"codex-{node}.jsonl")
    stream_path.parent.mkdir(parents=True, exist_ok=True)
    err_path = stream_path.with_suffix(".stderr")

    env = dict(os.environ)
    if codex_home:
        env["CODEX_HOME"] = str(codex_home)
        refresh_grunt_credential(Path(codex_home))
    env.setdefault("LEAN_NUM_THREADS", str(config.lean_threads))

    inner = [
        CODEX, "exec", "--json",
        "--skip-git-repo-check",
        # codex's OWN sandbox is bypassed because bwrap is the sandbox
        # here: nesting the two would deny the agent the file and network
        # access it needs inside a boundary that is already drawn tighter
        # than codex's own. The containment is `build_codex_agent_command`
        # below, and it is not optional — an unsandboxed shell agent with
        # approvals bypassed is a strictly larger hole than the one the
        # sidecar's compile sandbox exists to close.
        "--dangerously-bypass-approvals-and-sandbox",
        "-m", config.model_name,
    ]
    effort = (config.reasoning_effort or "").strip()
    if effort and effort != "none":
        inner += ["-c", f"reasoning_effort={effort}"]
    inner.append("-")

    from trellis.sidecar.workspace import build_codex_agent_command

    argv = build_codex_agent_command(
        repo,
        inner,
        lean_threads=config.lean_threads,
        allow_unsandboxed=config.allow_unsandboxed,
        burst_home=Path(codex_home) if codex_home else None,
    )

    timed_out = False
    codex_rc: Optional[int] = None
    # Stream straight to disk so a wall-clock kill still leaves every event
    # emitted before it.
    with stream_path.open("wb") as out, err_path.open("wb") as errf:
        proc = subprocess.Popen(
            argv,
            cwd=str(repo),
            stdin=subprocess.PIPE,
            stdout=out,
            stderr=errf,
            env=env,
            start_new_session=True,
        )
        try:
            assert proc.stdin is not None
            proc.stdin.write(task.encode("utf-8"))
            proc.stdin.close()
            codex_rc = proc.wait(timeout=wall)
        except subprocess.TimeoutExpired:
            timed_out = True
            # The agent spawns lake/lean children; killing the process
            # group is the only way to take the whole tree down, and
            # `start_new_session=True` above is what makes the group ours
            # to kill.
            try:
                os.killpg(proc.pid, signal.SIGTERM)
                proc.wait(timeout=20)
            except Exception:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except Exception:
                    pass
            codex_rc = None
        except Exception as exc:  # pragma: no cover - defensive
            result.detail = f"codex spawn failed: {exc}"
            result.wall_secs = now() - started
            return result

    stream = parse_codex_stream(stream_path)
    usage = stream["usage"] or {}
    usage_source = "turn.completed"
    if not usage:
        # Wall-killed: recover the last snapshot the session rollout kept.
        usage = rollout_usage_fallback(
            Path(codex_home) if codex_home else None, stream.get("thread_id")
        )
        usage_source = "rollout_fallback" if usage else "none"
    result.prompt_tokens = int(usage.get("input_tokens") or 0)
    result.completion_tokens = int(
        (usage.get("output_tokens") or 0) + (usage.get("reasoning_output_tokens") or 0)
    )
    # A CLI agent has no notion of a propose/compile loop's "iterations";
    # its shell calls are the closest honest analogue and keep the
    # telemetry column meaningful rather than always zero.
    # `iterations` counted turns of the retired driver's propose/compile loop.
    # This arm has no such loop, so it stays 0 rather than being fudged to
    # mean shell calls — a column that means two different things in two
    # arms is worse than a column that is honestly empty. codex activity
    # is logged below and the full event stream is on disk beside the
    # attempt log.
    result.iterations = 0
    log(
        f"codex {config.model_name}: {stream['command_calls']} shell call(s), "
        f"{stream['agent_messages']} message(s), "
        f"{result.prompt_tokens + result.completion_tokens} tokens "
        f"(usage from {usage_source})"
    )

    # ---- harvest, then restore -------------------------------------
    try:
        edited = node_file.read_text()
    except OSError as exc:
        result.detail = f"could not read node file after codex: {exc}"
        result.wall_secs = now() - started
        return result

    # A destroyed `-- BODY` marker is an agent DISCIPLINE failure, but
    # `split_body_marker` raises on zero or two markers — and an agent
    # writing `-- BODY` inside an explanatory comment is entirely natural,
    # since the prompt uses that exact string. Unguarded, the ValueError
    # escapes the runner and is reported as infrastructure ("attempt runner
    # crashed"), which is the opposite misclassification to the one below
    # and feeds the same daemon circuit breaker.
    try:
        prefix_now, body_now = split_body_marker(edited)
    except ValueError as exc:
        try:
            node_file.write_text(node_content)
        except OSError:
            pass
        result.status = "failed"
        result.detail = sanitize_detail(f"the -- BODY marker was destroyed: {exc}")
        result.wall_secs = now() - started
        return result
    touched = [p for p in _dirty_paths(repo) if p not in pre_dirty]
    collateral = [p for p in touched if p != f"Tablet/{node}.lean"]

    # Restore before scoring, so the workspace is exactly as the HTTP
    # driver would have left it regardless of which branch we take below.
    try:
        node_file.write_text(node_content)
    except OSError:
        pass
    if collateral:
        _git(repo, "checkout", "--", "Tablet/")
        _git(repo, "clean", "-fd", "--", "Tablet/")

    result.wall_secs = now() - started

    if collateral:
        result.status = "failed"
        result.detail = sanitize_detail(
            "edited files outside its node: " + ", ".join(sorted(collateral)[:5])
        )
        log(f"codex grunt discipline failure on {node}: {result.detail}")
        return result

    if prefix_now != original_prefix:
        result.status = "failed"
        result.detail = "frozen prefix above -- BODY was modified"
        log(f"codex grunt tampered with the frozen prefix on {node}")
        return result

    # Order matters for diagnosis. An untouched body still carries the
    # opening `sorry`, so a ban scan first would report every do-nothing
    # attempt as "banned token: sorry" — which reads as the agent having
    # written one deliberately. `detail` is the field failures are
    # diagnosed from, so the accurate reason wins.
    if body_now.strip() == initial_body.strip():
        # An unchanged body has TWO very different causes, and conflating
        # them is expensive. A model that tried and got nowhere is a
        # mathematical `failed`, which spends the node's queue generation.
        # A codex-side outage — expired auth, quota refusal, rate limit,
        # unknown model, `codex` unresolvable inside bwrap — exits non-zero
        # within seconds with an empty stream, and MUST be `error`:
        # `error` is the only status the daemon treats as transport, so it
        # is what preserves the generation and arms the circuit breaker.
        # Reported as `failed`, a single expired token silently drains the
        # whole queue one node at a time, and the reviewer reads each as a
        # hard node.
        infra = (
            (codex_rc not in (0, None))
            or int(stream.get("stream_lines") or 0) == 0
            or int(stream.get("agent_messages") or 0) == 0
        )
        if infra and not timed_out:
            result.status = "error"
            stderr_tail = ""
            try:
                stderr_tail = err_path.read_text(errors="replace").strip()[-500:]
            except OSError:
                pass
            result.detail = (
                f"codex produced nothing (rc={codex_rc}, "
                f"{stream.get('stream_lines') or 0} stream line(s), "
                f"{stream.get('agent_messages') or 0} message(s))"
                + (f": {sanitize_detail(stderr_tail)}" if stderr_tail else "")
            )
            log(f"codex infrastructure failure on {node}: {result.detail}")
            return result
        result.status = "budget_exhausted" if timed_out else "failed"
        # `previous_errors` is the last round's compile error, and at this
        # point it is stored NOWHERE else — the bare marker overwrote it in
        # the ledger, so 49 of 163 attempts read as having produced nothing
        # when each in fact left a diagnosable failed proof. Preserve it.
        marker = "body unchanged" + (" (wall budget)" if timed_out else "")
        result.detail = (
            sanitize_detail(f"{marker}; last error: {previous_errors}")
            if previous_errors
            else marker
        )
        # An unchanged body ends the ROUND, not the attempt. On a retry the
        # agent reverting its failed experiments back to the staged body is
        # ordinary hygiene, and treating it as terminal forfeited a median
        # 506s of the 900s wall. The loop's existing budgets (round floor,
        # backstop, wall) bound the retries; a wall kill stays terminal.
        result.proof_body = body_now
        result.retryable = not timed_out
        return result

    banned = body_ban_scan(body_now)
    if banned:
        result.status = "failed"
        result.detail = sanitize_detail(f"banned token in body: {banned}")
        return result

    # ---- the authoritative verdict ---------------------------------
    # codex's own claim of success is discarded here. This is the same
    # callback the HTTP driver compiles through, so both arms are scored
    # by identical code.
    #
    # The callback is built HERE rather than passed in ready-made: making
    # it opens a warm lean server on this workspace, and codex has been
    # running `lake build` in that same tree until a moment ago. Opening
    # it earlier would have the two contending over `.lake`. Deferring
    # also means every rejection above — collateral edit, prefix
    # tampering, banned token, no-op — costs no server at all.
    check_started = now()
    verdict = compile_body_factory()(body_now)
    result.check_secs = now() - check_started
    result.api_secs = max(0.0, result.wall_secs - result.check_secs)
    result.proof_body = body_now

    if getattr(verdict, "ok", False):
        result.status = "success"
        result.detail = "closed" + (" (after wall kill)" if timed_out else "")
        return result

    result.status = "budget_exhausted" if timed_out else "failed"
    # `CompileVerdict` carries the compiler output on `.log` — there is no
    # `detail`/`message` field. Reading for those silently discarded every
    # compile error and left `codex rc=0` as the only diagnostic.
    #
    # `extract_compiler_errors` (not a tail window) because two log shapes
    # arrive here and the errors sit at opposite ends of them — see its
    # docstring for the `grind` case that produced an unreadable detail.
    compiler_log = (getattr(verdict, "log", "") or "").strip()
    result.detail = ("wall budget; " if timed_out else "") + (
        extract_compiler_errors(compiler_log) if compiler_log
        else f"codex rc={codex_rc}, empty compile log"
    )
    # A disciplined body that does not yet build: exactly the failure the
    # retry loop can act on. Not when the wall is already gone.
    result.retryable = bool(compiler_log) and not timed_out
    return result


# Spawning codex with a sliver of wall left buys a turn that is killed
# before it can build anything. Below this, stop and keep the last failure.
MIN_ROUND_SECONDS = 120.0
# A BACKSTOP again, not the binding budget. Capping at 2 was measured and
# wrong: it fired on 38 of 60 attempts and left a MEDIAN 506s of the 900s
# wall unspent — 56% of a budget already paid for. The codex turn
# self-terminates at ~170s, so the wall affords roughly five turns and the
# cap was taking two. The wall and a round count are redundant budgets and
# the tighter one binds silently, which is exactly what happened.
#
# The earlier reasoning ("successes only ever arrived on round 1") came
# from a sample drawn entirely from the small-node population; it says
# nothing about how many rounds a harder node needs, and it was used to
# cut a budget rather than to spend it.
MAX_ATTEMPT_ROUNDS = 8


def run_attempt_codex(
    *,
    config: SidecarConfig,
    repo: Path,
    node: str,
    node_content: str,
    system_prompt: str,
    initial_body: str,
    compile_body_factory: Callable[[], Callable[[str], Any]],
    stream_path: Optional[Path] = None,
    codex_home: Optional[Path] = None,
    log: Callable[[str], None] = lambda _m: None,
    now: Callable[[], float] = time.monotonic,
) -> AttemptResult:
    """Propose/verify rounds until the body compiles or the wall is gone.

    `codex exec` is ONE turn: when the model decides it is finished, the
    turn ends whether or not the body builds. Without a loop around it a
    non-compiling final body ended the attempt outright, and the measured
    consequence was that grunts returned failures having spent a small
    fraction of the budget they were given — `twisty` closed at 157s of
    900s with the words "I couldn't complete a compiling proof within the
    available time", and no attempt on record had ever reached even the
    old 600s cap. Nothing re-prompted them.

    So the harness owns the loop: compile the body OURSELVES, and on a
    compile failure hand the compiler's errors back for another turn
    against the remaining wall.
    The agent resumes from its own failed body — it is left on disk for
    the retry — rather than starting over.

    Compile failures round-trip, and so does an unchanged body — on a
    retry the agent reverting failed experiments back to the staged body
    is ordinary sign-off hygiene, and ending the attempt there was
    measured to forfeit a median 506s of the 900s wall (49 of 163
    attempts, all on retry rounds). A collateral edit, a tampered prefix,
    a destroyed marker, a banned token or a codex-side outage is terminal
    on the first round, unchanged.
    """
    started = now()
    wall = float(config.attempt_wall_seconds)
    original_prefix, _ = split_body_marker(node_content)
    node_file = _tablet_path(repo, node)
    base_stream = (
        Path(stream_path) if stream_path else (Path(repo).parent / f"codex-{node}.jsonl")
    )

    result: AttemptResult = AttemptResult(status="error")
    base_body = initial_body
    previous_errors: Optional[str] = None
    prompt_tokens = completion_tokens = 0
    api_secs = check_secs = 0.0

    for round_index in range(1, MAX_ATTEMPT_ROUNDS + 1):
        # The wall is the AGENT's budget, so the harness's own verification
        # compile does not come out of it. `check_secs` reached a 115s mean
        # and an 884s max (one attempt spent its whole wall inside our
        # check), and a cold olean closure could consume an entire retry
        # round before the agent got a token — `lacFnbrs` round 1 spent 532s
        # of 650s in the harness check, leaving round 2 only 250s.
        remaining = wall - (now() - started - check_secs)
        if round_index > 1 and remaining < MIN_ROUND_SECONDS:
            log(
                f"retry stopped: {remaining:.0f}s left, under the "
                f"{MIN_ROUND_SECONDS:.0f}s floor for another turn"
            )
            break
        if round_index > 1:
            log(f"retry round {round_index}: {remaining:.0f}s of wall left")
        result = _attempt_round(
            config=config,
            repo=repo,
            node=node,
            node_content=node_content,
            system_prompt=system_prompt,
            initial_body=base_body,
            compile_body_factory=compile_body_factory,
            # Per-round stream so a retry never overwrites the telemetry of
            # the round before it; round 1 keeps the historical filename.
            stream_path=base_stream if round_index == 1
            else base_stream.with_suffix(f".r{round_index}.jsonl"),
            codex_home=codex_home,
            log=log,
            now=now,
            wall=max(remaining, 0.0),
            previous_errors=previous_errors,
        )
        # Budgets are cumulative across rounds; the caller ledgers ONE
        # attempt, so per-round figures would under-report the spend.
        prompt_tokens += result.prompt_tokens
        completion_tokens += result.completion_tokens
        api_secs += result.api_secs
        check_secs += result.check_secs
        result.prompt_tokens = prompt_tokens
        result.completion_tokens = completion_tokens
        result.api_secs = api_secs
        result.check_secs = check_secs
        result.wall_secs = now() - started
        result.iterations = round_index

        if result.status == "success" or not result.retryable:
            return result
        if round_index == MAX_ATTEMPT_ROUNDS:
            log(f"retry stopped: {MAX_ATTEMPT_ROUNDS}-round backstop reached")
            break
        # Hand the agent back its own failed body to iterate on. The round
        # restored the file to the ORIGINAL content before scoring, so this
        # re-applies the candidate; the frozen prefix is re-used verbatim,
        # never the agent's copy of it.
        #
        # An UNCHANGED round produced no new candidate and no new compile
        # verdict, so the next round keeps the last REAL compiler error as
        # its prompt context — never the "body unchanged; ..." marker — and
        # the staged body stays as it was. Strip-inequality is exactly the
        # round's own unchanged test: an unchanged body never reaches the
        # compile step, so inequality here means "new candidate, scored".
        if result.proof_body.strip() != base_body.strip():
            previous_errors = result.detail
            base_body = result.proof_body
        try:
            node_file.write_text(original_prefix + base_body)
        except OSError as exc:
            log(f"retry aborted: could not stage previous body ({exc})")
            break

    # Whatever the last round concluded, with the workspace already
    # restored by that round.
    try:
        node_file.write_text(node_content)
    except OSError:
        pass
    return result
