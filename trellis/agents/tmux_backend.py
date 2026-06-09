"""tmux-backed interactive-agent driver for Claude and Gemini.

One tmux session per logical lane. Prompts go in via `tmux send-keys` (with
automatic `tmux load-buffer` fallback for >4KB). Completion detection is
three-layer: done_file sentinel → apparent_stall zombie catcher → change-gated
pane stability. Authoritative output comes from the CLI's own transcript file,
not pane scraping.

Public entrypoint: `run(config, prompt, *, role, work_dir, ...) -> BurstResult`.
Called from `trellis.burst.run_{worker,reviewer}_burst` for
`config.provider in {"claude", "gemini"}`.
"""

from __future__ import annotations

import argparse
import atexit
import concurrent.futures
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import threading
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Sequence, Tuple

from trellis.adapters import BurstResult, ProviderConfig
from trellis.config import SandboxConfig
from trellis.host_runtime import worker_path_env
from trellis.sandbox import wrap_command as _project_wrap_command
from trellis.tmux_socket import tmux_argv as _tmux_argv


def _quota_probe_hook(
    provider: str,
    cwd: Path,
    *,
    burst_home: Optional[Path] = None,
) -> None:
    """Fire a cooldown-gated quota snapshot. NEVER raises.

    Imported lazily to keep tmux_backend importable in environments where
    the quota module hasn't been deployed yet (defense-in-depth: a missing
    quota module must not block bursts).
    """
    try:
        from trellis.quota_snapshots import maybe_probe_at_burst_boundary
    except Exception:
        return
    try:
        maybe_probe_at_burst_boundary(
            provider, cwd, burst_home=burst_home,
        )
    except Exception:
        pass


# --- per-burst parallel quota probes ---------------------------------------
#
# Burst dispatchers submit a forced quota probe (`probe_for_burst`) at the
# start of the burst and another at the end, both onto this executor. The
# pre-probe runs in parallel with the burst itself — its 10-25s wall cost
# adds 0 to wall-clock for any burst that runs longer than that. After the
# burst completes we await both with short timeouts; either may resolve to
# None (timeout, suspended provider, or any internal failure) and that's
# fine — the cost-ledger row simply records `quota_*: None`.
#
# Bound max_workers so a flood of concurrent panel members of mixed providers
# never spawns unbounded threads. 4 is enough: at most one pre + one post
# in-flight per provider, and three providers (claude/codex/gemini) means
# ~6 simultaneous probes is the worst realistic case; 4 caps it hard while
# staying above the steady-state need.

_PROBE_EXECUTOR: Optional[concurrent.futures.ThreadPoolExecutor] = None
_PROBE_EXECUTOR_LOCK = threading.Lock()


def _get_probe_executor() -> Optional[concurrent.futures.ThreadPoolExecutor]:
    """Lazy module-level executor. Returns None on construction failure
    (caller treats that as "no parallel probes available" and skips them)."""
    global _PROBE_EXECUTOR
    try:
        with _PROBE_EXECUTOR_LOCK:
            if _PROBE_EXECUTOR is None:
                _PROBE_EXECUTOR = concurrent.futures.ThreadPoolExecutor(
                    max_workers=4,
                    thread_name_prefix="trellis-quota-probe",
                )
            return _PROBE_EXECUTOR
    except Exception:
        return None


def _submit_probe_for_burst(
    provider: str,
    cwd: Path,
    *,
    burst_home: Optional[Path] = None,
) -> Optional[concurrent.futures.Future]:
    """Submit a forced (non-cooldown-gated) quota probe to the executor.

    Returns the Future, or None if the executor is unavailable or the
    quota module can't be imported. NEVER raises — quota tracking is
    cosmetic and must not break a run.
    """
    try:
        from trellis.quota_snapshots import probe_for_burst as _probe_for_burst
    except Exception:
        return None
    pool = _get_probe_executor()
    if pool is None:
        return None
    try:
        return pool.submit(
            _probe_for_burst,
            provider, cwd,
            burst_home=burst_home,
        )
    except Exception:
        return None


def _await_with_short_timeout(
    fut: Optional[concurrent.futures.Future],
    max_wait_seconds: float,
) -> Optional[Dict[str, Any]]:
    """Await a probe future with a hard wall-clock cap. Returns the payload
    on success, None on timeout, cancellation, or any exception. NEVER raises.
    """
    if fut is None:
        return None
    try:
        return fut.result(timeout=max(0.0, float(max_wait_seconds)))
    except concurrent.futures.TimeoutError:
        return None
    except Exception:
        return None


def _project_quota_for_ledger(
    payload: Optional[Dict[str, Any]],
    provider: str,
) -> Optional[Dict[str, Any]]:
    """Compact-project a probe payload for stamping on a cost-ledger row.
    Wraps the quota_snapshots helper in try/except so a stale/missing quota
    module never blocks a burst."""
    try:
        from trellis.quota_snapshots import quota_projection_for_ledger
    except Exception:
        return None
    try:
        return quota_projection_for_ledger(payload, provider)
    except Exception:
        return None

# --- structured lifecycle event log ----------------------------------------

_EVENT_LOG_LOCK = threading.Lock()
_DEFAULT_EVENT_LOG_PATH: Optional[Path] = None


def set_event_log_path(path: Optional[Path]) -> None:
    """Set the global default event-log destination. None disables logging."""
    global _DEFAULT_EVENT_LOG_PATH
    _DEFAULT_EVENT_LOG_PATH = path


def emit_event(
    event: str,
    *,
    session: Optional[str] = None,
    provider: Optional[str] = None,
    role: Optional[str] = None,
    scope: Optional[str] = None,
    attempt: Optional[int] = None,
    detail: Optional[dict] = None,
    log_path: Optional[Path] = None,
) -> None:
    """Append one JSONL event. Safe no-op if no log path configured."""
    target = log_path or _DEFAULT_EVENT_LOG_PATH
    if target is None:
        return
    record = {
        "ts": time.time(),
        "event": event,
    }
    if session is not None: record["session"] = session
    if provider is not None: record["provider"] = provider
    if role is not None: record["role"] = role
    if scope is not None: record["scope"] = scope
    if attempt is not None: record["attempt"] = attempt
    if detail: record["detail"] = detail
    line = json.dumps(record, default=str) + "\n"
    with _EVENT_LOG_LOCK:
        target.parent.mkdir(parents=True, exist_ok=True)
        with target.open("a", encoding="utf-8") as f:
            f.write(line)


# --- tmux server health ----------------------------------------------------

class BudgetExceededError(RuntimeError):
    """Raised when the cost ledger total exceeds a configured budget cap."""


class TmuxServerError(RuntimeError):
    """Raised when the tmux server is unreachable."""


def tmux_server_alive() -> bool:
    try:
        proc = subprocess.run(
            _tmux_argv("list-sessions", "-F", "#{session_name}"),
            capture_output=True, text=True, timeout=5, check=False,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return False
    if proc.returncode != 0:
        err = (proc.stderr or "").lower()
        # "no server running" is a specific tmux message indicating no server exists.
        # Empty session list (returncode 0) means server is alive but no sessions.
        # Returncode != 0 with "no server" means server is down.
        if "no server" in err or "error connecting" in err:
            return False
    return True


def ensure_tmux_server() -> None:
    """Preflight check — raises TmuxServerError with clear guidance if the server is dead."""
    if tmux_server_alive():
        return
    # Try to start a trivial detached session to spawn the server.
    proc = subprocess.run(
        _tmux_argv("start-server"),
        capture_output=True, text=True, timeout=5, check=False,
    )
    if not tmux_server_alive():
        raise TmuxServerError(
            f"tmux server unreachable (stderr={proc.stderr!r}). "
            "Ensure `tmux start-server` succeeds and $TMUX_TMPDIR / /tmp have free space."
        )


# --- CLI version detection -------------------------------------------------

def claude_cli_version(burst_home: Optional[Path] = None) -> str:
    """Query `claude --version`. Returns '' on failure.

    Post-bwrap-only: runs as supervisor; no sudo wrap. `burst_home` is
    accepted for symmetry but only affects PATH if set.
    """
    cmd = ["claude", "--version"]
    if burst_home is not None:
        cmd = ["env", f"HOME={burst_home}", f"PATH={worker_path_env(burst_home)}", *cmd]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=10, check=False)
        return proc.stdout.strip() or proc.stderr.strip()
    except Exception:
        return ""


def _gemini_user_prefix_bin(burst_home: Path) -> str:
    """Per-burst-user gemini install bin dir; prepended to PATH if present.

    Installed by ensure_gemini_cli_updated via `npm install --prefix <dir> -g`;
    yields `<dir>/bin/gemini` which takes precedence over /usr/bin/gemini.
    """
    return f"{burst_home}/.trellis-npm/bin"


def gemini_path_env(burst_home: Path) -> str:
    """PATH env value for gemini launches, including the per-user gemini bin.

    Delegates to `worker_path_env`, which already prepends
    `<burst_home>/.trellis-npm/bin` (the supervisor-managed install) and
    `<burst_home>/.local/share/npm-global/bin` (the conventional per-user
    npm-global install) ahead of the system PATH.
    """
    return worker_path_env(burst_home)


def gemini_cli_version(burst_home: Optional[Path] = None) -> str:
    cmd = ["gemini", "--version"]
    if burst_home is not None:
        cmd = [
            "env", f"HOME={burst_home}",
            f"PATH={gemini_path_env(burst_home)}",
            # Suppress update-notifier on version checks too — otherwise this
            # probe can race gemini's own logs.json writer mid-flush and
            # produce `logs.json.invalid_json.*.bak` churn.
            "NO_UPDATE_NOTIFIER=1",
            "GEMINI_CLI_DISABLE_UPDATE_CHECK=1",
            *cmd,
        ]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=10, check=False)
        return proc.stdout.strip() or proc.stderr.strip()
    except Exception:
        return ""


_GEMINI_UPDATE_LOCK = threading.Lock()
_GEMINI_UPDATE_CHECKED: set[str] = set()


def _semver_tuple(v: str) -> Optional[Tuple[int, ...]]:
    """Parse X.Y.Z (ignoring any -pre tag) into a tuple for comparison."""
    if not v:
        return None
    head = v.strip().split("-", 1)[0].split("+", 1)[0]
    parts = head.split(".")
    try:
        return tuple(int(p) for p in parts if p.isdigit())
    except Exception:
        return None


def _npm_latest_gemini_version(timeout: float = 15.0) -> str:
    try:
        proc = subprocess.run(
            ["npm", "view", "@google/gemini-cli", "version"],
            capture_output=True, text=True, timeout=timeout, check=False,
        )
        return proc.stdout.strip()
    except Exception:
        return ""


_GEMINI_UPDATE_STAMP_NAME = ".last-checked-at"
_GEMINI_UPDATE_STAMP_TTL_SECONDS = 6 * 3600  # 6 hours — err on the fresh side


def _gemini_update_stamp_path(burst_home: Path) -> Path:
    return burst_home / ".trellis-npm" / _GEMINI_UPDATE_STAMP_NAME


def _gemini_update_stamp_fresh(burst_home: Path) -> bool:
    """Return True if we successfully checked for a gemini update within TTL.

    The stamp lives under `<burst_home>/.trellis-npm/`. Post-bwrap-only:
    supervisor reads the stamp directly (no sudo).
    """
    stamp = _gemini_update_stamp_path(burst_home)
    try:
        ts = float(stamp.read_text(encoding="utf-8").strip())
        return (time.time() - ts) < _GEMINI_UPDATE_STAMP_TTL_SECONDS
    except Exception:
        return False


def _gemini_update_stamp_write(burst_home: Path) -> None:
    stamp = _gemini_update_stamp_path(burst_home)
    try:
        stamp.parent.mkdir(parents=True, exist_ok=True)
        stamp.write_text(f"{time.time():.0f}\n", encoding="utf-8")
    except Exception:
        pass


def ensure_gemini_cli_updated(
    burst_home: Optional[Path],
    *,
    force: bool = False,
) -> None:
    """Install/upgrade gemini under the burst's npm prefix if the system
    install is stale vs the npm registry.

    Fast-path idempotent:
      - In-process: a set keyed on burst_home skips rechecks within one process.
      - Across processes: a file stamp at
        `<burst_home>/.trellis-npm/.last-checked-at` with a 6-hour TTL
        skips the `npm view` network probe, which otherwise adds 5-15 s
        to every supervisor startup.

    Pass `force=True` to bypass both caches.
    """
    if burst_home is None:
        return
    cache_key = str(burst_home)
    with _GEMINI_UPDATE_LOCK:
        if (not force) and cache_key in _GEMINI_UPDATE_CHECKED:
            return
        home = burst_home
        # Cross-process TTL cache: if we checked recently, trust it.
        if (not force) and _gemini_update_stamp_fresh(home):
            _GEMINI_UPDATE_CHECKED.add(cache_key)
            return
        latest = _npm_latest_gemini_version()
        if not latest:
            _GEMINI_UPDATE_CHECKED.add(cache_key)
            return
        installed = gemini_cli_version(burst_home=home)
        inst_tuple = _semver_tuple(installed)
        latest_tuple = _semver_tuple(latest)
        if inst_tuple and latest_tuple and inst_tuple >= latest_tuple:
            # Already up-to-date — write stamp so future processes skip the
            # network probe until TTL expires.
            _gemini_update_stamp_write(home)
            _GEMINI_UPDATE_CHECKED.add(cache_key)
            return
        emit_event(
            "gemini_cli_upgrade_start",
            provider="gemini",
            detail={"burst_home": str(home), "installed": installed, "latest": latest},
        )
        prefix = f"{home}/.trellis-npm"
        try:
            proc = subprocess.run(
                [
                    "env", f"HOME={home}",
                    f"PATH={worker_path_env(home)}",
                    "bash", "-c",
                    f"mkdir -p {prefix} && npm install --prefix {prefix} -g @google/gemini-cli@latest",
                ],
                capture_output=True, text=True, timeout=300, check=False,
            )
            emit_event(
                "gemini_cli_upgrade_done",
                provider="gemini",
                detail={
                    "burst_home": str(home),
                    "returncode": proc.returncode,
                    "installed_after": gemini_cli_version(burst_home=home),
                    "tail": (proc.stdout + proc.stderr)[-500:],
                },
            )
        except Exception as e:
            emit_event(
                "gemini_cli_upgrade_failed",
                provider="gemini",
                detail={"burst_home": str(home), "error": str(e)[:200]},
            )
        # Stamp either way: if the install succeeded we're now up-to-date;
        # if it failed, don't re-try every single burst — 6-hour TTL gives
        # room to investigate.
        _gemini_update_stamp_write(home)
        _GEMINI_UPDATE_CHECKED.add(cache_key)


# --- driver lifecycle tracking for cleanup ---------------------------------

_OWNED_SESSIONS_LOCK = threading.Lock()
_OWNED_SESSIONS: set[str] = set()

# Serializes concurrent mutations of ~/.gemini/settings.json and
# ~/.gemini/trustedFolders.json when multiple gemini lanes launch in parallel.
_GEMINI_SETTINGS_LOCK = threading.Lock()

# Forces a minimum gap between concurrent gemini launches in the same process.
# Gemini shares ~/.gemini/ state across concurrent instances; launching two
# within <1s triggers intermittent startup failures (one or both may restart
# mid-flight, dropping the prompt). A 1.5s stagger reliably avoids this.
_GEMINI_LAUNCH_LOCK = threading.Lock()
_GEMINI_LAST_LAUNCH_AT = 0.0
_GEMINI_LAUNCH_MIN_GAP = 1.5


def _gemini_launch_stagger() -> None:
    global _GEMINI_LAST_LAUNCH_AT
    with _GEMINI_LAUNCH_LOCK:
        gap = time.monotonic() - _GEMINI_LAST_LAUNCH_AT
        if gap < _GEMINI_LAUNCH_MIN_GAP:
            time.sleep(_GEMINI_LAUNCH_MIN_GAP - gap)
        _GEMINI_LAST_LAUNCH_AT = time.monotonic()


def _remember_owned_session(name: str) -> None:
    with _OWNED_SESSIONS_LOCK:
        _OWNED_SESSIONS.add(name)


def _forget_owned_session(name: str) -> None:
    with _OWNED_SESSIONS_LOCK:
        _OWNED_SESSIONS.discard(name)


def _atexit_cleanup_owned_sessions() -> None:
    with _OWNED_SESSIONS_LOCK:
        names = list(_OWNED_SESSIONS)
    for name in names:
        try:
            subprocess.run(
                _tmux_argv("kill-session", "-t", name),
                capture_output=True, text=True, timeout=5, check=False,
            )
        except Exception:
            pass


atexit.register(_atexit_cleanup_owned_sessions)


def _install_signal_handlers() -> None:
    """Install SIGTERM/SIGINT handlers that clean up sessions before exiting."""
    def _handler(signum: int, _frame) -> None:
        try:
            emit_event("driver_signal", detail={"signum": signum})
        except Exception:
            pass
        try:
            _atexit_cleanup_owned_sessions()
        except Exception:
            pass
        # Use os._exit to avoid recursive cleanup via atexit.
        os._exit(128 + int(signum))
    for s in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        try:
            prev = signal.getsignal(s)
            # Don't clobber the default SIGINT if we're imported from a REPL.
            if prev in (signal.SIG_DFL, None):
                signal.signal(s, _handler)
        except (ValueError, OSError):
            # May fail in non-main threads; ignore.
            pass


_install_signal_handlers()


def sweep_stale_trellis_sessions(*, older_than_seconds: float = 7200.0) -> List[str]:
    """Sweep tmux sessions named `trellis-*` older than the cutoff.

    Returns the list of session names killed. Safe to call periodically
    from the bridge even if the driver is otherwise healthy.
    """
    now = time.time()
    proc = subprocess.run(
        _tmux_argv("list-sessions", "-F", "#{session_name} #{session_created}"),
        capture_output=True, text=True, timeout=10, check=False,
    )
    killed: list[str] = []
    for line in proc.stdout.splitlines():
        parts = line.strip().split()
        if len(parts) != 2:
            continue
        name, created_s = parts
        if not name.startswith("trellis-"):
            continue
        try:
            created = float(created_s)
        except ValueError:
            continue
        if (now - created) < older_than_seconds:
            continue
        subprocess.run(
            _tmux_argv("kill-session", "-t", name),
            capture_output=True, text=True, timeout=5, check=False,
        )
        killed.append(name)
    return killed


# ---- tmux primitives -------------------------------------------------------

def tmux(*args: str, check: bool = False, timeout: int = 15) -> subprocess.CompletedProcess:
    """Run a tmux command. TimeoutExpired is caught and returned as a failed
    CompletedProcess (returncode=-1) so callers can handle it gracefully
    rather than crashing under server contention (6+ concurrent panes
    hammering `display-message` / `capture-pane`)."""
    argv = _tmux_argv(*args)
    try:
        return subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=check,
        )
    except subprocess.TimeoutExpired as exc:
        return subprocess.CompletedProcess(
            args=argv,
            returncode=-1,
            stdout="",
            stderr=f"tmux timeout after {timeout}s: {' '.join(args)}: {exc}",
        )


def has_session(name: str) -> bool:
    return tmux("has-session", "-t", name).returncode == 0


def per_lane_tmpdir(session_name_str: str, *, under: Optional[Path] = None) -> Path:
    """Per-lane TMPDIR so concurrent agents don't cross-pollute /tmp.

    Each agent sees a dedicated scratch dir for its transient files
    (terminal state, etc).
    """
    root = under or Path("/tmp/tmux-agent-exp/agent-tmpdirs")
    root.mkdir(parents=True, exist_ok=True)
    path = root / session_name_str
    path.mkdir(parents=True, exist_ok=True)
    return path


def new_session(
    name: str,
    *,
    cwd: Path,
    cmd: List[str],
    width: int = 220,
    height: int = 60,
    extra_env: Optional[dict] = None,
    isolate_tmpdir: bool = True,
) -> None:
    if has_session(name):
        raise RuntimeError(f"tmux session {name!r} already exists")
    # tmux `-e KEY=VAL` sets env vars on the child. Start with inherited env.
    env_args: list[str] = []
    if isolate_tmpdir:
        tmp = per_lane_tmpdir(name)
        for k in ("TMPDIR", "TMP", "TEMP"):
            env_args.extend(["-e", f"{k}={tmp}"])
    if extra_env:
        for k, v in extra_env.items():
            env_args.extend(["-e", f"{k}={v}"])
    tmux(
        "new-session", "-d",
        "-s", name,
        "-c", str(cwd),
        "-x", str(width),
        "-y", str(height),
        *env_args,
        *cmd,
        check=True,
        timeout=10,
    )
    _remember_owned_session(name)


def kill_session(name: str) -> None:
    if has_session(name):
        tmux("kill-session", "-t", name)
    _forget_owned_session(name)


def send_keys(name: str, *keys: str) -> None:
    tmux("send-keys", "-t", name, *keys, check=True, timeout=5)


def capture(name: str, *, history: bool = True) -> str:
    # -p print to stdout; -J join wrapped lines; -e preserve escape sequences? we want them stripped
    args = ["capture-pane", "-t", name, "-p", "-J"]
    if history:
        args.extend(["-S", "-"])  # include all scrollback
    out = tmux(*args, timeout=5)
    return out.stdout


def pane_pid(name: str) -> Optional[int]:
    out = tmux("list-panes", "-t", name, "-F", "#{pane_pid}", timeout=5)
    if out.returncode != 0:
        return None
    try:
        return int(out.stdout.strip().splitlines()[0])
    except Exception:
        return None


def pane_dead(name: str) -> bool:
    """Return True if the pane's child has exited OR the tmux session is gone.

    A transport timeout (returncode -1 from our tmux wrapper) is NOT treated
    as dead — it just means we couldn't query this poll. An actual tmux
    error ("can't find pane", "no server") IS treated as dead so wait_until_idle
    returns promptly when a session is killed externally.
    """
    out = tmux("display-message", "-p", "-t", name, "#{pane_dead}", timeout=15)
    if out.returncode == -1:
        # Our wrapper's timeout sentinel. Assume still alive; next poll retries.
        return False
    if out.returncode != 0:
        err = (out.stderr or "").lower()
        if "can't find" in err or "no server" in err or "no such" in err:
            return True
        # Unknown non-zero (e.g., tmux bug) — don't falsely kill the burst.
        return False
    return out.stdout.strip() == "1"


# ---- text normalization ----------------------------------------------------

_ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")  # CSI
_ANSI_OSC_RE = re.compile(r"\x1b\][^\x07]*\x07")     # OSC ... BEL
_ANSI_OTHER = re.compile(r"\x1b[PX^_].*?\x1b\\")     # DCS/PM/APC/SOS

# Spinner / busy glyphs seen on claude and gemini.
_SPINNER_GLYPHS = set(
    "⠁⠂⠃⠄⠅⠆⠇⠈⠉⠊⠋⠌⠍⠎⠏⠐⠑⠒⠓⠔⠕⠖⠗⠘⠙⠚⠛⠜⠝⠞⠟"
    "⠠⠡⠢⠣⠤⠥⠦⠧⠨⠩⠪⠫⠬⠭⠮⠯⠰⠱⠲⠳⠴⠵⠶⠷⠸⠹⠺⠻⠼⠽⠾⠿"
    "⣀⣁⣂⣃⣄⣅⣆⣇⣈⣉⣊⣋⣌⣍⣎⣏⣐⣑⣒⣓⣔⣕⣖⣗⣘⣙⣚⣛⣜⣝⣞⣟"
    "⣠⣡⣢⣣⣤⣥⣦⣧⣨⣩⣪⣫⣬⣭⣮⣯⣰⣱⣲⣳⣴⣵⣶⣷⣸⣹⣺⣻⣼⣽⣾⣿"
    "|/-\\"
    "◐◓◑◒◜◝◞◟"
    "✦✧✶✷✸✹"
)


def strip_ansi(text: str) -> str:
    text = _ANSI_RE.sub("", text)
    text = _ANSI_OSC_RE.sub("", text)
    text = _ANSI_OTHER.sub("", text)
    return text


def strip_spinner_chars(text: str) -> str:
    return "".join(ch for ch in text if ch not in _SPINNER_GLYPHS)


def normalize_pane(text: str, *, also_strip_spinners: bool = True) -> str:
    """Normalize a pane snapshot for progress/stability comparison.

    Strips everything that's PURE cosmetic animation, so pane diffs only fire
    on REAL content changes:
      - ANSI escape sequences
      - Spinner glyphs (⠙⠹⠸… |/-\\ etc)
      - Entire footer line containing the busy hint ("esc to interrupt", etc)
      - Timer patterns like "12s" or "1m 23s" (remaining timers outside the footer)
      - Status-word + ellipsis ("Thinking…", "Bootstrapping…", "Combobulating…")
    """
    t = strip_ansi(text)
    if also_strip_spinners:
        t = strip_spinner_chars(t)
    # Replace entire lines containing the busy hint with a sentinel.
    t = _BUSY_FOOTER_LINE_RE.sub("<BUSY_LINE>", t)
    # Strip any leftover timer patterns (e.g. "(1m 54s · ↓ 7 tokens)" where
    # the busy marker is on a different line; we want the token count to
    # still register as progress, but not the timer tick).
    t = _TIMER_LONG_RE.sub("<T>", t)
    t = _TIMER_SHORT_RE.sub("<T>", t)
    # Strip rotating "cute status words" followed by ellipsis.
    t = _STATUS_WORD_ELLIPSIS_RE.sub("<STATUS>", t)
    # Collapse trailing whitespace per line + trailing blank lines.
    lines = [ln.rstrip() for ln in t.splitlines()]
    while lines and lines[-1] == "":
        lines.pop()
    return "\n".join(lines)


# Any line containing a known busy-hint substring is pure animation.
_BUSY_FOOTER_LINE_RE = re.compile(
    r"^.*(?:esc to (?:interrupt|cancel)|ctrl-c to interrupt).*$",
    re.IGNORECASE | re.MULTILINE,
)
# Timer patterns: "1m 54s", "12s" (near parens / whitespace boundaries).
_TIMER_LONG_RE = re.compile(r"\b\d+m\s+\d+s\b")
_TIMER_SHORT_RE = re.compile(r"(?<=[\s(·,])\d+s\b")
# Claude/gemini cycle through "Thinking…", "Bootstrapping…", "Combobulating…".
# Match a capitalized word of 4-20 letters immediately followed by three dots
# or the ellipsis character.
_STATUS_WORD_ELLIPSIS_RE = re.compile(r"\b[A-Z][a-z]{3,19}(?:\.{3}|…)")


# Busy-indicator heuristics. Agent is working if any of these strings are in the
# stripped-ansi (but NOT spinner-stripped) screen text.
_CLAUDE_BUSY_MARKERS = (
    "esc to interrupt",
    "ctrl-c to interrupt",
    "(esc ",      # "(esc or ctrl-c to …)"
)
_GEMINI_BUSY_MARKERS = (
    "esc to cancel",
    "(press ctrl+c",
    # Gemini shows a "✦ Agent is thinking" or similar at the bottom
    "is thinking",
    "Responding",
)

# Terminal-error markers rendered by the gemini-cli interactive UI after
# its own internal retries are exhausted. Bundle source confirms the
# canonical UI prefix `[API Error: ...]` (bundle/chunk-6DSAZLFF.js renders
# `[API Error: ${msg}]`, `[API Error: ${msg} (Status: 5xx)]`, etc).
# When this prefix sits in the pane AND the CLI is no longer busy (no
# "is thinking" / "Responding" / "esc to cancel"), the worker is wedged
# at idle with a failed turn and will never write the done_file. Catch it
# fast instead of waiting the full stable_after_busy window (5400 s).
_GEMINI_API_ERROR_MARKERS = (
    "[API Error:",
)

# Gemini-cli's "Allow execution of [<cmd>]?" permission prompt blocks even
# in --approval-mode=yolo (observed on gemini-cli 0.39.1 for `rm`). We
# detect it from the pane and inject "1" (the highlighted "Allow once"
# choice) to unblock. Captured fixture:
#   tests/fixtures/gemini-permission-prompts/20260424-111230-rm-permission-plain.txt
_GEMINI_PERMISSION_PROMPT_RE = re.compile(
    r"Allow execution of \[([^\]\n]+)\]\?[^\n]*\n"
    r"(?:[^\n]*\n){0,3}"
    r"\s*(?:\(checked\)\s*)?1\.\s*Allow",
    re.MULTILINE,
)

# Gemini-cli's "Loop detected" interactive prompt. Triggered when the
# CLI's repeat-tool-call detector flags suspected looping. Observed on
# gemini-cli 0.39.1 with gemini-auto when an agent iterates on syntactic
# variants of the same Lean rewrite and gemini halts to confirm. We send
# "2" (disable loop detection for
# this session) so a false-positive flag doesn't terminate an
# otherwise-progressing burst. If the loop was real, the worker will
# either still produce useful work or get caught by the supervisor's
# stable-without-done-file timeout.
_GEMINI_LOOP_DETECTED_PROMPT_RE = re.compile(
    r"Loop detected[^\n]*\n"
    r"(?:[^\n]*\n){0,4}?"
    r"\s*(?:\(checked\)\s*)?1\.\s*Keep loop detection enabled[^\n]*\n"
    r"\s*(?:\(checked\)\s*)?2\.\s*Disable loop detection for this session",
    re.MULTILINE,
)


def detect_gemini_permission_prompt(screen: str) -> Optional[str]:
    """Return the command being prompted for, or None if no prompt active."""
    plain = strip_ansi(screen)
    match = _GEMINI_PERMISSION_PROMPT_RE.search(plain)
    return match.group(1).strip() if match else None


def detect_gemini_loop_detected_prompt(screen: str) -> bool:
    """True iff the pane shows gemini-cli's Loop-detected confirmation prompt."""
    return _GEMINI_LOOP_DETECTED_PROMPT_RE.search(strip_ansi(screen)) is not None


def _gemini_auto_confirm_log_path(cwd: Path) -> Path:
    return cwd / ".trellis" / "logs" / "gemini-auto-confirm.jsonl"


def _log_gemini_auto_confirm(
    cwd: Path,
    *,
    session: str,
    command: str,
    pane_tail: str,
) -> None:
    path = _gemini_auto_confirm_log_path(cwd)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        record = {
            "ts": time.time(),
            "session": session,
            "command": command,
            "pane_tail": pane_tail[-2000:],
        }
        with path.open("a", encoding="utf-8") as f:
            f.write(json.dumps(record) + "\n")
    except OSError:
        pass


def maybe_auto_confirm_gemini_prompt(
    handle: "AgentHandle",
    screen: str,
) -> Optional[str]:
    """If the pane shows a gemini interactive confirmation prompt we
    recognize, dispatch the right keystroke and return a label for the
    dismissed prompt. Otherwise return None.

    Currently handled:
      - "Allow execution of [<cmd>]?" permission prompt — send "1" to
        accept the highlighted "Allow once" choice; returns the bracketed
        command.
      - "Loop detected" prompt — send "2" to disable loop detection for
        this session (rather than the default "1" which halts the burst);
        returns the sentinel label "loop_detected:disable".

    Caller should sleep briefly after firing so the next pane capture
    sees the dismissed prompt instead of the same frame.
    """
    if handle.provider != "gemini":
        return None
    plain_screen = strip_ansi(screen)

    # Permission prompt: "1" = Allow once.
    command = detect_gemini_permission_prompt(screen)
    if command is not None:
        try:
            send_keys(handle.session, "1")
        except Exception:
            return None
        _log_gemini_auto_confirm(
            handle.cwd,
            session=handle.session,
            command=command,
            pane_tail=plain_screen,
        )
        return command

    # Loop-detected prompt: "2" = Disable loop detection for this
    # session. Sending "1" (the default) terminates the burst with
    # "request has been halted", which is the wrong default for a
    # cycle that can productively continue if the loop flag was a
    # false positive. Fixture:
    # tests/fixtures/gemini-permission-prompts/20260512-loop-detected.txt
    if detect_gemini_loop_detected_prompt(screen):
        try:
            send_keys(handle.session, "2")
        except Exception:
            return None
        label = "loop_detected:disable"
        _log_gemini_auto_confirm(
            handle.cwd,
            session=handle.session,
            command=label,
            pane_tail=plain_screen,
        )
        return label

    return None


def agent_is_busy(screen: str, provider: str) -> bool:
    # Only strip ANSI; keep spinners so we can spot the busy region.
    t = strip_ansi(screen).lower()
    if provider == "claude":
        return any(m in t for m in (marker.lower() for marker in _CLAUDE_BUSY_MARKERS))
    if provider == "gemini":
        return any(m in t for m in (marker.lower() for marker in _GEMINI_BUSY_MARKERS))
    return False


def agent_has_terminal_api_error(screen: str, provider: str) -> Optional[str]:
    """Return the matched marker if the pane shows a terminal API error, else
    None.

    Currently gemini-only. The gemini-cli interactive UI renders failed-turn
    errors as `[API Error: ...]` after its own internal retries are
    exhausted; once that text appears and the CLI returns to the idle
    prompt (no busy marker), the burst will never produce a done_file.

    Callers should still gate on `not agent_is_busy` + persistence across
    several polls before acting, since gemini-cli briefly flashes the same
    marker between its internal retries (which DO eventually succeed).
    """
    if provider != "gemini":
        return None
    plain = strip_ansi(screen)
    for marker in _GEMINI_API_ERROR_MARKERS:
        if marker in plain:
            return marker
    return None


# ---- high-level driver -----------------------------------------------------

@dataclass
class AgentHandle:
    session: str
    cwd: Path
    provider: str  # "claude" or "gemini"
    session_id: str = ""  # claude uuid if used
    log_dir: Path = field(default_factory=lambda: Path("/tmp/tmux-agent-exp/logs"))
    done_file: Optional[Path] = None

    def log(self, msg: str) -> None:
        self.log_dir.mkdir(parents=True, exist_ok=True)
        line = f"[{time.strftime('%H:%M:%S')}] {self.session}: {msg}\n"
        (self.log_dir / f"{self.session}.log").write_text(
            (self.log_dir / f"{self.session}.log").read_text() + line
            if (self.log_dir / f"{self.session}.log").exists() else line,
            encoding="utf-8",
        )
        sys.stderr.write(line)

    def snapshot(self) -> str:
        return capture(self.session)


def _claude_input_line_is_empty(norm: str) -> bool:
    """Last non-empty normalized line should be `❯` alone (possibly with box bars)."""
    for line in reversed(norm.splitlines()):
        s = line.strip()
        if not s:
            continue
        # strip box-drawing / bypass-permissions footer
        if s.startswith("⏵⏵") or s.startswith("──") or s.startswith("──"):
            continue
        if "❯" in s:
            # After ❯ the box is "empty/ready" when nothing is typed. But claude
            # renders a dim, ROTATING placeholder suggestion in the empty box
            # (e.g. `Try "fix lint errors"`, `Try "how do I log an error?"` —
            # the suggestion cycles); tmux capture strips the dim color, so the
            # placeholder looks like typed text. Treat any `Try "..."`
            # placeholder, whichever suggestion is showing, as empty. A real
            # prompt we send never has this shape.
            after = s.split("❯", 1)[1].strip()
            if after == "":
                return True
            return bool(re.fullmatch(r'Try ".*"', after))
        return False
    return False


def _gemini_input_line_is_empty(norm: str) -> bool:
    """Gemini: input placeholder present + no 'User:' line staged for the current turn.

    Works for both the default Ink TUI (has ▀▀▀/▄▄▄ borders) and --screen-reader
    mode (plain text). The placeholder 'Type your message or @path/to/file' is
    shown in both modes when the input is empty.
    """
    if "Type your message" not in norm:
        return False
    # If response is still streaming, the screen-reader view will have
    # "Model: " with content following the last "User:" — we want to see
    # only after streaming finished. But 'idle' gating is done separately;
    # here we just confirm the input box is rendered.
    return True


def wait_until_prompt_ready(h: AgentHandle, *, timeout: float = 60.0, probe_every: float = 0.5) -> bool:
    """Wait until TUI is idle with an empty input box ready for our prompt.

    Success criterion: the "empty input box" heuristic is true across two
    consecutive polls (cheap, works for both cold start and between turns).
    """
    deadline = time.monotonic() + timeout
    consecutive_ready = 0
    while time.monotonic() < deadline:
        try:
            screen = capture(h.session, history=False)
        except subprocess.CalledProcessError:
            time.sleep(probe_every)
            continue
        norm = normalize_pane(screen)
        if h.provider == "claude":
            box_empty = _claude_input_line_is_empty(norm)
        elif h.provider == "gemini":
            box_empty = _gemini_input_line_is_empty(norm)
        else:
            box_empty = False
        if box_empty:
            consecutive_ready += 1
            if consecutive_ready >= 2:
                return True
        else:
            consecutive_ready = 0
        time.sleep(probe_every)
    return False


def _workspace_latest_mtime_ns(paths: Sequence[Path]) -> int:
    """Return the highest mtime_ns across all regular files below each path.

    Swallow OSError for paths that vanish mid-walk. Returns 0 if nothing found.
    """
    best = 0
    for root in paths:
        if not root.exists():
            continue
        # Quickly check the root's own mtime in case it's a single file.
        try:
            s = root.stat()
            if s.st_mtime_ns > best:
                best = s.st_mtime_ns
        except OSError:
            pass
        if not root.is_dir():
            continue
        for current, dirs, files in os.walk(root, followlinks=False):
            # Skip noisy vcs/tmp dirs
            dirs[:] = [d for d in dirs if d not in {".git", "__pycache__", "node_modules"}]
            for name in files:
                p = Path(current) / name
                try:
                    s = p.stat()
                    if s.st_mtime_ns > best:
                        best = s.st_mtime_ns
                except OSError:
                    pass
    return best


def wait_until_idle(
    h: AgentHandle,
    *,
    min_stable_seconds: float = 6.0,
    poll_interval: float = 1.0,
    # INACTIVITY timeout: resets ONLY on real progress (pane diff after
    # ANSI+spinner strip, or FS change in workspace_paths). A lone busy
    # marker ("esc to interrupt" with a frozen timer) is NOT progress —
    # it's the zombie state we want to catch. Default 30 min of true silence.
    total_timeout: float = 1800.0,
    done_file: Optional[Path] = None,
    require_change_first: bool = True,
    baseline: Optional[str] = None,
    # Apparent-stall detector: fires when the agent shows a busy marker but
    # NO positive work signal has fired for apparent_stall_seconds. Positive
    # work signals are unforgeable by a TUI-render thread:
    #   (1) `workspace_paths`: latest mtime_ns of files under those paths
    #       bumps when the agent tool-calls (WriteFile, lake build, etc).
    #   (2) `liveness_probe`: caller-supplied int-returning callable
    #       (typically agent_session_transcript_mtime_ns) whose return value
    #       grows when the agent's session-transcript file is appended by a
    #       streamed token chunk or tool_use block.
    # TUI tickers (busy-line timer, spinner glyphs) are NOT used: they can
    # keep incrementing even when the reasoning thread is wedged, because
    # the TUI-render loop often runs on a separate async task.
    workspace_paths: Optional[Sequence[Path]] = None,
    liveness_probe: Optional[Callable[[], int]] = None,
    apparent_stall_seconds: float = 300.0,
    # Terminal-API-error detector (gemini): once the gemini-cli interactive
    # UI renders `[API Error: ...]` AND the CLI is no longer busy (no
    # "is thinking"/"Responding"), it's wedged at idle and will never write
    # a done_file. Catch it after `api_error_idle_seconds` of continuous
    # presence (default 60 s) so a transient flash between gemini's own
    # internal retries doesn't false-positive. 0 disables the check.
    api_error_idle_seconds: float = 60.0,
) -> Tuple[bool, str]:
    """Wait for the agent burst to end.

    Four signals are checked on each poll, in priority order:
      1. `done_file` exists              → done_file
      2. pane died                       → pane_dead (fail)
      3. gemini pane shows `[API Error: ...]` continuously while NOT busy
         for `api_error_idle_seconds`    → api_error_idle (fail)
      4. agent-busy marker NOT present AND normalized pane has been stable
         for `min_stable_seconds`        → stable

    `require_change_first` = True means stability only counts after we first
    observe a post-baseline change, so a dropped prompt never "stabilizes"
    on the input banner.
    """
    start = time.monotonic()
    last_norm = baseline
    last_change = time.monotonic()
    change_seen = not require_change_first
    ever_busy = False
    ws_paths = tuple(workspace_paths or ())
    last_seen_fs_mtime = _workspace_latest_mtime_ns(ws_paths) if ws_paths else 0
    # apparent-stall is active whenever threshold > 0.
    apparent_stall_enabled = apparent_stall_seconds > 0
    # Liveness tracking — POSITIVE work signals only. TUI tickers / spinners
    # are NOT used because they can keep rendering from a TUI-render thread
    # while the reasoning thread is wedged.
    last_seen_probe_mtime = liveness_probe() if liveness_probe is not None else 0
    last_liveness_ns = time.monotonic_ns()
    # Inactivity-based timeout: total_timeout measures wall time since last
    # REAL progress (pane diff after spinner-strip, or FS change). A busy
    # marker alone does NOT count — that's the zombie case.
    last_inactivity_reset = time.monotonic()
    # Terminal-API-error tracking. `api_error_first_seen` is the monotonic
    # time when the gemini `[API Error:` marker was first observed in the
    # continuous-presence window. Reset to None whenever the CLI is busy or
    # the marker disappears (gemini-cli internally retries: marker flickers
    # in/out between attempts; we only want to act when the marker has been
    # continuously present AND the CLI is idle, meaning retries are done).
    api_error_enabled = api_error_idle_seconds > 0
    api_error_first_seen: Optional[float] = None

    while time.monotonic() - last_inactivity_reset < total_timeout:
        if done_file is not None and done_file.exists():
            return True, "done_file"
        if pane_dead(h.session):
            return False, "pane_dead"
        screen = capture(h.session)
        # gemini-cli's "Allow execution of [<cmd>]?" permission prompt
        # blocks even in --approval-mode=yolo for some commands. Auto-press
        # "1" (the highlighted "Allow once") to keep the burst moving.
        # Counts as positive work signal so the inactivity timer resets.
        if h.provider == "gemini":
            confirmed = maybe_auto_confirm_gemini_prompt(h, screen)
            if confirmed is not None:
                last_inactivity_reset = time.monotonic()
                last_liveness_ns = time.monotonic_ns()
                # Gemini's UI takes ~1s to update after the keystroke;
                # re-poll on the next iteration with fresh state.
                time.sleep(1.0)
                continue
        norm = normalize_pane(screen)
        busy = agent_is_busy(screen, h.provider)
        # Terminal-API-error fast path: a wedged-at-idle gemini pane will
        # never reach `done_file`, and the existing stable_after_busy
        # detector takes 90 min + the pre-stable busy span to fire
        # (on the order of 14 000 s end-to-end in practice).
        # If the API-error marker is visible AND the CLI is not busy,
        # require continuous presence for `api_error_idle_seconds` before
        # failing — gemini-cli's own retry loop briefly flashes the same
        # marker between attempts, and we don't want to short-circuit a
        # retry that would have succeeded.
        if api_error_enabled and not busy:
            marker = agent_has_terminal_api_error(screen, h.provider)
            if marker is not None:
                now = time.monotonic()
                if api_error_first_seen is None:
                    api_error_first_seen = now
                elif now - api_error_first_seen >= api_error_idle_seconds:
                    return False, f"api_error_idle_{api_error_idle_seconds:g}s"
            else:
                api_error_first_seen = None
        else:
            # Busy means gemini is retrying internally — reset the window.
            api_error_first_seen = None
        # FS activity in workspace (tool-call writes: WriteFile, scratch,
        # Tablet, staging, lake build).
        if ws_paths:
            cur_fs = _workspace_latest_mtime_ns(ws_paths)
            fs_changed = cur_fs > last_seen_fs_mtime
            if fs_changed:
                last_seen_fs_mtime = cur_fs
        else:
            fs_changed = False
        # Session transcript growth (provider-backed session JSONL/JSON
        # file in the burst-user's home — gets appended every time the
        # agent receives a streamed token chunk or tool_use block).
        if liveness_probe is not None:
            cur_probe = liveness_probe()
            probe_changed = cur_probe > last_seen_probe_mtime
            if probe_changed:
                last_seen_probe_mtime = cur_probe
        else:
            probe_changed = False
        if fs_changed or probe_changed:
            last_liveness_ns = time.monotonic_ns()
        # Pane activity for PROGRESS (resets inactivity timer):
        # normalize_pane strips spinner + timer + busy line, so this fires
        # only on real content changes.
        pane_changed = (last_norm is not None and norm != last_norm)

        # Only REAL progress resets the inactivity timer — busy marker alone
        # (frozen spinner, static TUI) does not.
        if pane_changed or fs_changed:
            last_inactivity_reset = time.monotonic()

        if busy:
            ever_busy = True
            # Zombie detection: busy marker visible AND no positive work
            # signal (workspace FS write OR session-transcript growth) for
            # apparent_stall_seconds → the agent's reasoning thread is
            # wedged even if the TUI is still rendering. Fail fast so
            # max_restarts can recover.
            if apparent_stall_enabled:
                frozen_for = (time.monotonic_ns() - last_liveness_ns) / 1e9
                if frozen_for >= apparent_stall_seconds:
                    return False, f"apparent_stall_{apparent_stall_seconds:g}s"
            # Don't count this as stable — even if busy without progress,
            # the stability timer is meaningless while the agent claims busy.
            last_change = time.monotonic()
            change_seen = True
            last_norm = norm
            time.sleep(poll_interval)
            continue
        if last_norm is None:
            last_norm = norm
            last_change = time.monotonic()
            time.sleep(poll_interval)
            continue
        if pane_changed:
            last_norm = norm
            last_change = time.monotonic()
            change_seen = True
        elif change_seen and (time.monotonic() - last_change) >= min_stable_seconds:
            return True, f"stable_{min_stable_seconds:g}s" + ("_after_busy" if ever_busy else "")
        time.sleep(poll_interval)
    if not change_seen:
        return False, "no_change_seen"
    return False, "timeout"


def baseline_snapshot(h: AgentHandle) -> str:
    return normalize_pane(capture(h.session))


def launch_claude(
    cwd: Path,
    *,
    session_id: str,
    model: Optional[str] = None,
    name_hint: str = "claude-test",
    extra_args: Optional[List[str]] = None,
    width: int = 220,
    height: int = 60,
) -> AgentHandle:
    cwd.mkdir(parents=True, exist_ok=True)
    session = f"trellis-{name_hint}"
    cmd = [
        "claude",
        "--dangerously-skip-permissions",
        "--session-id", session_id,
    ]
    if model:
        cmd.extend(["--model", model])
    if extra_args:
        cmd.extend(extra_args)
    kill_session(session)  # clean slate
    new_session(session, cwd=cwd, cmd=cmd, width=width, height=height)
    return AgentHandle(
        session=session, cwd=cwd, provider="claude", session_id=session_id,
    )


# ---- session identity sidecar ----------------------------------------------

def _session_sidecar_path(
    cwd: Path, *, provider: str, role: str, session_scope: str
) -> Path:
    scope_slug = _SLUG_RE.sub("-", str(session_scope or "")).strip("-._") or "default"
    return cwd / ".trellis" / "sessions" / f"{provider}-{role}-{scope_slug}.json"


def _session_identity_payload(
    *, provider: str, model: Optional[str], effort: Optional[str],
    session_scope: str, extra: Optional[List[str]] = None,
) -> dict:
    return {
        "provider": provider,
        "model": (model or "").strip(),
        "effort": (effort or "").strip() if provider == "claude" else "",
        "session_scope": str(session_scope or ""),
        "extra": sorted(str(x) for x in (extra or [])),
    }


def load_session_identity(
    cwd: Path, *, provider: str, role: str, session_scope: str
) -> Optional[dict]:
    path = _session_sidecar_path(cwd, provider=provider, role=role, session_scope=session_scope)
    if not path.is_file():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return None


def store_session_identity(
    cwd: Path, *, provider: str, role: str, session_scope: str,
    session_id: Optional[str], identity: dict,
) -> None:
    path = _session_sidecar_path(cwd, provider=provider, role=role, session_scope=session_scope)
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {"session_id": session_id or "", "identity": identity}
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def identities_match(a: dict, b: dict) -> bool:
    """Strict equality on all identity fields."""
    return dict(a) == dict(b)


def clear_session_identity(
    cwd: Path, *, provider: str, role: str, session_scope: str
) -> None:
    _session_sidecar_path(cwd, provider=provider, role=role, session_scope=session_scope).unlink(missing_ok=True)


# ---- cost reporting / ledger ----------------------------------------------

# Claude model pricing (USD per 1M tokens). Source: Anthropic's public pricing.
# Kept here so we can compute cost from interactive-transcript usage (which
# doesn't include cost_usd — only -p --output-format=json does).
# Update when Anthropic changes pricing.
_CLAUDE_PRICING_USD_PER_MTOK = {
    "claude-opus-4-7":       {"input": 15.00, "output": 75.00, "cache_read": 1.50, "cache_write": 18.75},
    "claude-opus-4-6":       {"input": 15.00, "output": 75.00, "cache_read": 1.50, "cache_write": 18.75},
    "claude-sonnet-4-6":     {"input": 3.00,  "output": 15.00, "cache_read": 0.30, "cache_write": 3.75},
    "claude-haiku-4-5-20251001": {"input": 1.00, "output": 5.00, "cache_read": 0.10, "cache_write": 1.25},
}


def claude_cost_usd(usage: dict) -> Optional[float]:
    """Compute cost from a claude transcript usage dict. Returns None if model unknown."""
    if not isinstance(usage, dict):
        return None
    model = str(usage.get("model", "") or "")
    pricing = _CLAUDE_PRICING_USD_PER_MTOK.get(model)
    if pricing is None:
        return None
    it = int(usage.get("input_tokens", 0) or 0)
    ot = int(usage.get("output_tokens", 0) or 0)
    cr = int(usage.get("cache_read_input_tokens", 0) or 0)
    cw = int(usage.get("cache_creation_input_tokens", 0) or 0)
    cost = (
        it * pricing["input"]
        + ot * pricing["output"]
        + cr * pricing["cache_read"]
        + cw * pricing["cache_write"]
    ) / 1_000_000.0
    return round(cost, 6)


# Gemini pricing is plan-based (Code Assist subscription). We record token
# counts from the transcript but don't attempt USD conversion for gemini.
def gemini_cost_usd(usage: dict) -> Optional[float]:
    return None


# Codex (OpenAI Responses-API) pricing (USD per 1M tokens). Rates as published
# by OpenAI; update when pricing changes. `input_tokens` from the codex CLI's
# turn.completed event is the GROSS prompt count (includes cached portion), so
# we bill (input - cached) at the input rate and cached at the cached rate.
_CODEX_PRICING_USD_PER_MTOK = {
    "gpt-5":          {"input": 1.25, "cached_input": 0.125, "output": 10.00},
    "gpt-5.4":        {"input": 1.25, "cached_input": 0.125, "output": 10.00},
    "gpt-5.5":        {"input": 1.25, "cached_input": 0.125, "output": 10.00},
    "gpt-5-codex":    {"input": 1.25, "cached_input": 0.125, "output": 10.00},
    "gpt-5-mini":     {"input": 0.25, "cached_input": 0.025, "output": 2.00},
    "gpt-5-nano":     {"input": 0.05, "cached_input": 0.005, "output": 0.40},
    "o3":             {"input": 2.00, "cached_input": 0.50,  "output": 8.00},
    "o4-mini":        {"input": 1.10, "cached_input": 0.275, "output": 4.40},
}


def codex_cost_usd(usage: dict) -> Optional[float]:
    """Compute USD cost from a codex turn.completed usage dict."""
    if not isinstance(usage, dict):
        return None
    model = str(usage.get("model", "") or "")
    pricing = _CODEX_PRICING_USD_PER_MTOK.get(model)
    if pricing is None:
        return None
    it = int(usage.get("input_tokens", 0) or 0)
    ct = int(usage.get("cached_input_tokens", 0) or 0)
    ot = int(usage.get("output_tokens", 0) or 0)
    new_input = max(0, it - ct)
    cost = (
        new_input * pricing["input"]
        + ct * pricing["cached_input"]
        + ot * pricing["output"]
    ) / 1_000_000.0
    return round(cost, 6)


def default_cost_ledger_path(cwd: Path) -> Path:
    return cwd / ".trellis" / "logs" / "cost-ledger.jsonl"


def _gemini_active_account(burst_home: Optional[Path]) -> Optional[str]:
    """Return the currently-active Google account email for gemini, or None.

    Reads `~/.gemini/google_accounts.json` under `burst_home` (or caller's
    own HOME if unset). Post-bwrap-only: no sudo path.
    """
    try:
        from trellis.gemini_accounts import active_account  # type: ignore
        result = active_account(burst_home=burst_home)
        if result:
            return result
    except Exception:
        pass
    # Standalone fallback.
    base = burst_home or Path.home()
    ga = base.resolve() / ".gemini" / "google_accounts.json"
    try:
        data = json.loads(ga.read_text(encoding="utf-8"))
        return str(data.get("active", "") or "").strip() or None
    except Exception:
        return None


def _gemini_try_rotate_to_new_account(
    *, burst_home: Optional[Path],
    exclude: Optional[Sequence[str]] = None,
) -> Optional[str]:
    """Attempt to switch to any gemini account not in exclude. Returns new email or None.

    Useful recovery on auth_expired (current account's OAuth token is dead)
    and on some rate-limit flavors (swap to a different account).
    """
    try:
        from trellis.gemini_accounts import (  # type: ignore
            active_account, available_accounts, rotation_available, switch_account,
        )
    except Exception:
        return None
    if not rotation_available(burst_home=burst_home):
        return None
    current = active_account(burst_home=burst_home)
    accounts = available_accounts(burst_home=burst_home)
    exclude_set = set(exclude or [])
    if current:
        exclude_set.add(current)
    for email in accounts:
        if email in exclude_set:
            continue
        if switch_account(email, burst_home=burst_home):
            return email
    return None


def _gemini_maybe_reroute_for_quota(
    *, burst_home: Optional[Path],
) -> Optional[str]:
    """Re-run the quota-aware rotation picker. Returns the active email after
    the check (may be unchanged). Uses the gemini /stats endpoint under the hood.
    """
    try:
        from trellis.gemini_accounts import ensure_budget  # type: ignore
        return ensure_budget(burst_home=burst_home)
    except Exception:
        return _gemini_active_account(burst_home)


_CUMULATIVE_USAGE_KEYS = (
    # claude / anthropic
    "input_tokens", "output_tokens",
    "cache_read_input_tokens", "cache_creation_input_tokens",
    # codex / openai — these are transcript-cumulative on every
    # `turn.completed` event, just like claude's. Must be listed here so
    # `append_cost_ledger`'s delta-subtraction in resumed sessions
    # subtracts them too (otherwise summing `usage.cached_input_tokens`
    # across rows over-counts by a large factor on long resumed sessions).
    "cached_input_tokens", "reasoning_output_tokens",
)


_CLAUDE_AUTH_CACHE: Dict[str, Tuple[float, dict]] = {}
_CLAUDE_AUTH_CACHE_TTL_SEC = 300.0


def _read_claude_subscription_tags(
    *,
    burst_home: Optional[Path] = None,
) -> dict:
    """Query `claude auth status --json` to get the burst's subscription tier.

    Returns a dict suitable for stamping on the cost-ledger row. Keys (all
    optional, omitted when unknown):
      - subscription_tier — `subscriptionType` from auth status (e.g. "max")
      - account           — `email` field
      - auth_method       — e.g. "claude.ai"

    Best-effort: returns {} on any failure. Cached per burst_home
    for 5 minutes — auth status doesn't change often, and we don't want to fork
    a `claude auth status` subprocess on every burst.

    Why not the settings.json file? On the user's spec'd schema `subscriptionType`
    lives there, but on this install it does not — the actual source is
    `claude auth status --json`, confirmed empirically.

    Note: the `rateLimitTier` / current-window state is NOT in auth-status JSON —
    that comes from per-burst `rate_limit_event` records in the transcript
    (captured separately).
    """
    cache_key = str(burst_home) if burst_home else ""
    now = time.monotonic()
    cached = _CLAUDE_AUTH_CACHE.get(cache_key)
    if cached and (now - cached[0]) < _CLAUDE_AUTH_CACHE_TTL_SEC:
        return dict(cached[1])

    cmd: List[str] = ["claude", "auth", "status", "--json"]
    if burst_home is not None:
        cmd = [
            "env",
            f"HOME={burst_home}",
            f"PATH={worker_path_env(burst_home)}",
            *cmd,
        ]
    try:
        proc = subprocess.run(
            cmd, capture_output=True, text=True, timeout=10, check=False,
        )
    except Exception:
        _CLAUDE_AUTH_CACHE[cache_key] = (now, {})
        return {}
    if proc.returncode != 0 or not proc.stdout.strip():
        _CLAUDE_AUTH_CACHE[cache_key] = (now, {})
        return {}
    try:
        data = json.loads(proc.stdout)
    except Exception:
        _CLAUDE_AUTH_CACHE[cache_key] = (now, {})
        return {}
    if not isinstance(data, dict):
        _CLAUDE_AUTH_CACHE[cache_key] = (now, {})
        return {}
    out: dict = {}
    sub = data.get("subscriptionType")
    if isinstance(sub, str) and sub:
        out["subscription_tier"] = sub
    auth_method = data.get("authMethod")
    if isinstance(auth_method, str) and auth_method:
        out["auth_method"] = auth_method
    email = data.get("email")
    if isinstance(email, str) and email:
        out["account"] = email
    _CLAUDE_AUTH_CACHE[cache_key] = (now, out)
    return out


def _count_claude_assistant_turns(
    work_dir: Path,
    session_id: str,
    *,
    home: Optional[Path] = None,
) -> Optional[int]:
    """Count assistant records in claude's transcript jsonl.

    Anthropic's subscription caps are expressed in messages-per-5-hour-window,
    so we record a per-burst turn count to enable those post-hoc rollups.

    Returns None if the transcript is unreadable; 0 if readable but empty.
    """
    path = claude_transcript_path(work_dir, session_id, home=home)
    text = _read_text_maybe_sudo(path)
    if text is None:
        return None
    count = 0
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("type") == "assistant":
            count += 1
    return count


def _count_gemini_turns(
    work_dir: Path,
    session_id: Optional[str] = None,
    *,
    home: Optional[Path] = None,
) -> Optional[int]:
    """Count gemini-type messages in the gemini session chat JSON."""
    rec = _gemini_read_transcript(
        work_dir, session_id, home=home
    )
    path = rec.get("path") or ""
    if not path:
        return None
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except Exception:
        return None
    msgs = data.get("messages", []) if isinstance(data, dict) else []
    return sum(1 for m in msgs if isinstance(m, dict) and m.get("type") == "gemini")


def _count_codex_turns(output_text: str) -> int:
    """Count `turn.completed` events in a codex burst's JSON stdout."""
    n = 0
    for line in output_text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            continue
        if rec.get("type") == "turn.completed":
            n += 1
    return n


def _ledger_prior_session_cumulative(
    path: Path, session_id: Optional[str]
) -> tuple[float, dict]:
    """Find the most recent ledger entry for this session_id and return its
    cumulative session cost and cumulative token usage.

    Returns (cumulative_cost_usd, {token_key: cumulative_count, ...}).
    Empty tuple (0.0, {}) if no prior entry or session_id is missing.

    For cost, prefers the explicit `session_total_cost_usd` column, falling
    back to `cost_usd` for legacy rows (where cost_usd was the cumulative).
    For tokens, prefers `session_total_usage`, falling back to `usage` legacy
    cumulative.
    """
    if not session_id or not path.is_file():
        return 0.0, {}
    latest_cost = 0.0
    latest_usage: dict = {}
    try:
        with path.open("r", encoding="utf-8") as f:
            for line in f:
                try:
                    d = json.loads(line)
                except Exception:
                    continue
                if d.get("session_id") != session_id:
                    continue
                c = d.get("session_total_cost_usd")
                if c is None:
                    c = d.get("cost_usd")
                if isinstance(c, (int, float)):
                    latest_cost = float(c)
                cum_u = d.get("session_total_usage")
                if not isinstance(cum_u, dict):
                    cum_u = d.get("usage") if isinstance(d.get("usage"), dict) else None
                if isinstance(cum_u, dict):
                    latest_usage = {
                        k: int(cum_u.get(k, 0) or 0)
                        for k in _CUMULATIVE_USAGE_KEYS
                    }
    except Exception:
        return 0.0, {}
    return latest_cost, latest_usage


def append_cost_ledger(
    cwd: Path,
    *,
    provider: str,
    role: str,
    scope: str,
    model: str,
    usage: Optional[dict],
    cost_usd: Optional[float],
    duration_seconds: float,
    attempts: int,
    ok: bool,
    reason: str,
    session_id: Optional[str] = None,
    account: Optional[str] = None,
    log_path: Optional[Path] = None,
    cumulative_cost_usd: Optional[float] = None,
    ts_start: Optional[float] = None,
    message_count: Optional[int] = None,
    subscription_tier: Optional[str] = None,
    rate_limit_tier: Optional[str] = None,
    extra: Optional[dict] = None,
    quota_pre: Optional[dict] = None,
    quota_post: Optional[dict] = None,
) -> None:
    """Append a cost record.

    `cost_usd`: the **per-burst** cost (delta) — what this single burst cost,
    regardless of any prior session history. Summing this column over the
    ledger gives a correct run total.

    `cumulative_cost_usd`: optional — the current session's running total
    (from the full resumed transcript). When provided AND `cost_usd` is None,
    the ledger auto-computes the delta against the most recent prior entry
    for this session_id. When both are provided, they're logged as-is.

    Historical note: claude's per-burst cost used to be written as the
    session-cumulative total because the transcript usage sum is cumulative.
    That inflated summarize_cost_ledger totals 4x over a 10-burst session.
    This helper + the updated claude call sites produce correct per-burst
    deltas AND preserve the cumulative for debugging.
    """
    path = log_path or default_cost_ledger_path(cwd)
    path.parent.mkdir(parents=True, exist_ok=True)
    # If caller gave us only a cumulative figure and no explicit per-burst
    # delta, compute delta here against the prior cumulative for this session.
    # Tokens in `usage` are also transcript-cumulative for claude/gemini, so
    # split them into a per-burst `usage` delta + a `session_total_usage`
    # snapshot, mirroring the cost split.
    effective_cost = cost_usd
    effective_cumulative = cumulative_cost_usd
    effective_usage = usage
    session_total_usage: Optional[dict] = None
    if effective_cost is None and effective_cumulative is not None:
        prior_cost, prior_usage = _ledger_prior_session_cumulative(path, session_id)
        # Guard against non-monotonic: cumulative should only grow; clamp to 0.
        effective_cost = max(0.0, effective_cumulative - prior_cost)
        if isinstance(usage, dict):
            session_total_usage = dict(usage)
            delta_usage = dict(usage)
            for k in _CUMULATIVE_USAGE_KEYS:
                cur = int(usage.get(k, 0) or 0)
                prev = int(prior_usage.get(k, 0) or 0)
                delta_usage[k] = max(0, cur - prev)
            effective_usage = delta_usage
    ts_end = time.time()
    effective_ts_start: Optional[float] = ts_start
    if effective_ts_start is None and duration_seconds is not None:
        # Best-effort reconstruction from the caller's duration — avoids a
        # protocol change when the burst site doesn't yet pass ts_start.
        try:
            effective_ts_start = ts_end - float(duration_seconds)
        except Exception:
            effective_ts_start = None
    rec = {
        "ts": ts_end,
        "ts_start": effective_ts_start,
        "provider": provider, "role": role, "scope": scope, "model": model,
        "account": account,  # gemini: email; claude: None (single OAuth)
        "ok": ok, "reason": reason, "attempts": attempts,
        "duration_seconds": round(duration_seconds, 3),
        "usage": effective_usage, "cost_usd": effective_cost,
        "session_total_cost_usd": effective_cumulative,
        "session_total_usage": session_total_usage,
        "session_id": session_id,
        # New: message/turn count — a burst can be many assistant turns.
        # Anthropic's subscription caps are per-message over a 5-hr window, so
        # this is what you need to total to estimate cap consumption.
        "message_count": message_count,
        # New: subscription / rate-limit tier tags, stamped once per burst.
        # For claude they come from ~/.claude/settings.json; for gemini we
        # pass None (no local equivalent has been located yet).
        "subscription_tier": subscription_tier,
        "rate_limit_tier": rate_limit_tier,
        # Per-burst quota probes (start/end). Compact projections produced by
        # `quota_projection_for_ledger`; either may be None when the bracketing
        # probe did not complete in its short timeout window. The viewer uses
        # the (pre, post) delta for per-burst USD attribution.
        "quota_pre": quota_pre,
        "quota_post": quota_post,
    }
    if isinstance(extra, dict):
        # Allow call sites to stamp provider-specific extras (e.g., codex
        # exit_code) without extending the signature further. Keys are
        # flattened into the row so downstream JSON consumers don't have to
        # walk a nested dict.
        for k, v in extra.items():
            if k not in rec:
                rec[k] = v
    with _EVENT_LOG_LOCK:  # reuse the log lock — cheap and safe
        with path.open("a", encoding="utf-8") as f:
            f.write(json.dumps(rec, default=str) + "\n")


def summarize_cost_ledger(path: Path) -> dict:
    """Read a ledger and return per-(provider, role, model, account) totals.

    For claude, `account` is typically None (single OAuth). For gemini,
    `account` is the Google email, so per-account quota consumption can be
    inspected independently.
    """
    if not path.is_file():
        return {"bursts": 0, "totals": []}
    recs = []
    for line in path.open("r", encoding="utf-8"):
        try:
            recs.append(json.loads(line))
        except Exception:
            continue
    totals_by_key: dict = {}
    for r in recs:
        key = (r.get("provider", ""), r.get("role", ""), r.get("model", ""), r.get("account") or "")
        bucket = totals_by_key.setdefault(key, {
            "count": 0, "ok_count": 0, "cost_usd": 0.0, "duration_seconds": 0.0,
            "input_tokens": 0, "output_tokens": 0, "cache_read": 0, "cache_write": 0,
            "attempts_sum": 0,
        })
        bucket["count"] += 1
        if r.get("ok"): bucket["ok_count"] += 1
        if isinstance(r.get("cost_usd"), (int, float)): bucket["cost_usd"] += r["cost_usd"]
        bucket["duration_seconds"] += float(r.get("duration_seconds", 0) or 0)
        bucket["attempts_sum"] += int(r.get("attempts", 0) or 0)
        u = r.get("usage") or {}
        if isinstance(u, dict):
            # Per-provider key conventions:
            #   claude: input_tokens / output_tokens / cache_read_input_tokens
            #           / cache_creation_input_tokens  (input_tokens = NEW input only)
            #   gemini: input / output / cached / thoughts / tool / total
            #   codex:  input_tokens / cached_input_tokens / output_tokens
            #           (input_tokens is GROSS — includes cached)
            provider = r.get("provider", "")
            raw_input = int(
                u.get("input_tokens", 0) or u.get("input", 0) or 0
            )
            cache_read = int(
                u.get("cache_read_input_tokens", 0)
                or u.get("cached_input_tokens", 0)
                or u.get("cached", 0)
                or 0
            )
            # For codex the API-reported input_tokens is GROSS (includes
            # cached); subtract to get the new-input count that matches the
            # claude semantics used elsewhere in the rollup.
            if provider == "codex":
                bucket["input_tokens"] += max(0, raw_input - cache_read)
            else:
                bucket["input_tokens"] += raw_input
            bucket["output_tokens"] += int(
                u.get("output_tokens", 0) or u.get("output", 0) or 0
            )
            bucket["cache_read"] += cache_read
            bucket["cache_write"] += int(u.get("cache_creation_input_tokens", 0) or 0)
    totals_list = [
        {"provider": k[0], "role": k[1], "model": k[2], "account": k[3] or None, **v}
        for k, v in sorted(totals_by_key.items())
    ]
    grand = {
        "cost_usd": round(sum(t["cost_usd"] for t in totals_list), 4),
        "bursts": sum(t["count"] for t in totals_list),
        "ok": sum(t["ok_count"] for t in totals_list),
        "duration_seconds": round(sum(t["duration_seconds"] for t in totals_list), 2),
    }
    return {"bursts": len(recs), "totals": totals_list, "grand": grand}


def summarize_gemini_by_account(path: Path) -> dict:
    """Totals broken out per gemini Google account."""
    summary = summarize_cost_ledger(path)
    per_account: dict = {}
    for t in summary["totals"]:
        if t["provider"] != "gemini":
            continue
        acc = t.get("account") or "<unknown>"
        bucket = per_account.setdefault(acc, {
            "count": 0, "ok_count": 0, "duration_seconds": 0.0,
            "input": 0, "output": 0, "cached": 0, "thoughts": 0,
        })
        bucket["count"] += t["count"]
        bucket["ok_count"] += t["ok_count"]
        bucket["duration_seconds"] += t["duration_seconds"]
        # Gemini token fields use different names; pull from raw usage records.
    # Walk recs once more for gemini-specific token fields.
    if path.is_file():
        for line in path.open("r", encoding="utf-8"):
            try:
                r = json.loads(line)
            except Exception:
                continue
            if r.get("provider") != "gemini":
                continue
            acc = r.get("account") or "<unknown>"
            u = r.get("usage") or {}
            if not isinstance(u, dict):
                continue
            bucket = per_account.setdefault(acc, {
                "count": 0, "ok_count": 0, "duration_seconds": 0.0,
                "input": 0, "output": 0, "cached": 0, "thoughts": 0,
            })
            for k in ("input", "output", "cached", "thoughts"):
                v = u.get(k)
                if isinstance(v, (int, float)):
                    bucket[k] += int(v)
    return per_account


# ---- chat-history artifact integration (viewer contract) -------------------

def write_chat_artifacts(
    cwd: Path,
    *,
    role: str,
    artifact_prefix: str,
    prompt: str,
    final_pane_text: str,
    transcript_last_assistant: str,
    transcript_path: Optional[Path] = None,
    cycle_dir: Optional[Path] = None,
    provider: Optional[str] = None,
    model: Optional[str] = None,
    session_id: Optional[str] = None,
    started_at_ms: Optional[int] = None,
    ended_at_ms: Optional[int] = None,
    request_id: Optional[int] = None,
    scope: Optional[str] = None,
    persistent_transcript_name: Optional[str] = None,
) -> Optional[Path]:
    """Populate .trellis/chats/<cycle>/<artifact>/{prompt.txt,result.txt,transcript,call.json}.

    Uses project's chat_history helpers if available. Falls back to direct writes
    when trellis isn't on PYTHONPATH (experiment mode).

    When ``persistent_transcript_name`` is set, the copied transcript is placed
    at that canonical filename (e.g. "transcript.jsonl", "transcript.json") in
    addition to its original name — so that git-committed history always contains
    a stable path the viewer can read regardless of provider session_id.
    """
    try:
        from trellis.chat_history import ensure_chat_file_link  # type: ignore
        have_project = True
    except Exception:
        ensure_chat_file_link = None  # type: ignore
        have_project = False

    chats_root = cwd / ".trellis" / "chats"
    if cycle_dir is None:
        cycle_dir = chats_root / "live"
    target = cycle_dir / f"{role}_{artifact_prefix}"
    target.mkdir(parents=True, exist_ok=True)
    (target / "prompt.txt").write_text(prompt, encoding="utf-8")
    (target / "pane.txt").write_text(final_pane_text, encoding="utf-8")
    if transcript_last_assistant:
        (target / "result.txt").write_text(transcript_last_assistant, encoding="utf-8")
    copied_bytes: Optional[bytes] = None
    if transcript_path:
        dst = target / transcript_path.name
        try:
            # Direct read (works if caller has filesystem permission).
            copied_bytes = transcript_path.read_bytes()
            dst.write_bytes(copied_bytes)
        except PermissionError:
            # Burst-user-owned transcripts (mode 700 home): read via sudo.
            # We don't know which user owns it from here; a best-effort sudo
            # as the path's stat owner would require extra plumbing, so we
            # try the common case of the repo's configured burst user.
            # If that fails too, silently skip — pane.txt + result.txt still
            # give debug value on the supervisor side.
            try:
                r = subprocess.run(
                    ["sudo", "-n", "cat", str(transcript_path)],
                    capture_output=True, timeout=15,
                )
                if r.returncode == 0 and r.stdout:
                    copied_bytes = r.stdout
                    dst.write_bytes(copied_bytes)
            except Exception:
                pass
        except Exception:
            pass
        # Also drop a canonical-name copy so viewer readers don't need to know
        # the provider-specific filename (session_id varies per burst).
        if copied_bytes is not None and persistent_transcript_name:
            try:
                (target / persistent_transcript_name).write_bytes(copied_bytes)
            except Exception:
                pass
    # Drop a `call.json` metadata sidecar so the viewer can resolve
    # provider/model/session without having to guess from dir names or parse
    # the full transcript.
    try:
        artifact_dir_name = f"{role}_{artifact_prefix}"
        call_meta: Dict[str, Any] = {
            "provider": provider,
            "model": model,
            "role": role,
            "session_id": session_id,
            "started_at_ms": int(started_at_ms) if started_at_ms is not None else None,
            "ended_at_ms": int(ended_at_ms) if ended_at_ms is not None else int(time.time() * 1000),
            "request_id": request_id,
            "artifact_id": artifact_dir_name,
            "scope": scope,
        }
        (target / "call.json").write_text(
            json.dumps(call_meta, indent=2) + "\n", encoding="utf-8"
        )
    except Exception:
        pass
    # If project helpers are available, also use their linker for the canonical name.
    if have_project and ensure_chat_file_link is not None:
        try:
            log_dir = cwd / ".trellis" / "logs" / "bursts"
            log_dir.mkdir(parents=True, exist_ok=True)
            linker_prompt = ensure_chat_file_link(
                cwd, log_dir=log_dir, artifact_prefix=artifact_prefix,
                role=role, log_filename=f"{artifact_prefix}-prompt.txt",
                canonical_name="prompt.txt",
            )
            linker_prompt.write_text(prompt, encoding="utf-8")
        except Exception:
            pass
    return target


# ---- rate-limit / capacity-error pattern matching --------------------------

# Patterns borrowed from trellis/burst.py:is_rate_limited (case-insensitive).
RATE_LIMIT_PATTERNS = (
    "rate limit", "rate_limit", "ratelimit", "too many requests", "429",
    "resource_exhausted", "model_capacity_exhausted", "quota exceeded",
    "usage limit", "credit balance is too low", "overloaded_error",
    "hit your limit", "exceeded retry limit",
)


def is_rate_limited(text: str) -> bool:
    lower = (text or "").lower()
    return any(pat in lower for pat in RATE_LIMIT_PATTERNS)


FAST_RETRYABLE_PATTERNS = (
    "agent died immediately after receiving prompt",
)


def is_fast_retryable(text: str) -> bool:
    lower = (text or "").lower()
    return any(pat in lower for pat in FAST_RETRYABLE_PATTERNS)


_EXHAUSTED_MODEL_RE = re.compile(r"No capacity available for model (\S+)", re.IGNORECASE)
_EXHAUSTED_MODEL_JSON_RE = re.compile(r'"model":\s*"([^"]+)"')


def extract_exhausted_model(text: str) -> Optional[str]:
    """Parse the exhausted model name from a MODEL_CAPACITY_EXHAUSTED-flavored error.

    Mirrors trellis.burst.extract_exhausted_model so this backend makes the
    same fallback decisions as the rest of the burst layer.
    """
    if not text:
        return None
    m = _EXHAUSTED_MODEL_RE.search(text)
    if m:
        return m.group(1)
    if "model_capacity_exhausted" in text.lower():
        m = _EXHAUSTED_MODEL_JSON_RE.search(text)
        if m:
            return m.group(1)
    return None


def exponential_backoff(base: float, attempt: int, max_delay: float) -> float:
    """Match trellis/burst.py semantics: min(base * 2**attempt, max_delay)."""
    return min(base * (2 ** max(0, attempt)), max_delay)


# ---- session naming --------------------------------------------------------

_SLUG_RE = re.compile(r"[^A-Za-z0-9_.-]+")


def session_name(provider: str, role: str, *, session_scope: str = "", extra: str = "") -> str:
    """Stable tmux session name for a logical agent lane.

    Format: trellis-<provider>-<role>[-<scope>][-<extra>], slugified.
    Each logical lane gets its own tmux session for isolation.
    """
    parts = ["trellis", provider, role]
    for s in (session_scope, extra):
        s = _SLUG_RE.sub("-", str(s or "")).strip("-._")
        if s:
            parts.append(s)
    name = "-".join(parts)
    # tmux session names cannot contain `:` or `.`; we only allow [A-Za-z0-9_-].
    return name[:120]


# ---- bwrap wrapper (mirrors trellis/sandbox.py:wrap_command) --------------

def _bwrap_available() -> bool:
    return shutil.which("bwrap") is not None


def sandbox_wrap(
    inner_cmd: List[str],
    *,
    enabled: bool,
    work_dir: Path,
    burst_home: Optional[Path],
    writable_paths: Optional[Sequence[Path]] = None,
    role: str = "worker",
    sandbox_config: Optional[SandboxConfig] = None,
) -> List[str]:
    """Delegate to the project's `trellis.sandbox.wrap_command` if enabled.

    `writable_paths` is ignored — the project helper computes the authoritative
    bind-mount surface from (repo, role). Kept in the signature for API
    compatibility with the standalone-experiment version.
    """
    if not enabled:
        return list(inner_cmd)
    if sandbox_config is None:
        # Fallback config when the caller didn't pass one — mirrors the
        # bwrap-backend default used by the project elsewhere.
        sandbox_config = SandboxConfig(enabled=True, backend="bwrap")
    return _project_wrap_command(
        list(inner_cmd),
        sandbox=sandbox_config,
        work_dir=work_dir,
        burst_home=burst_home,
        role=role,
    )


def claude_launch_settings_args() -> List[str]:
    """Settings passed via `claude --settings <json>` (flagSettings) on every
    burst launch. Both keys are confirmed against the claude binary's zod
    schema and verified live:

    - `prefersReducedMotion`: disable cycling animations (spinner shimmer,
      rotating status words, timer ticks) so a stuck TUI is distinguishable
      from a live one. `esc to interrupt` stays in the footer, so the busy
      detector still fires.
    - `skipDangerousModePermissionPrompt`: pre-accept claude's one-time
      "Bypass Permissions mode" disclaimer. Interactive
      `claude --dangerously-skip-permissions` shows it on a fresh machine and
      blocks headless. claude gates the disclaimer on this key across
      user/local/flag/policy settings (`pp()` in the binary), so passing it as
      flagSettings suppresses it regardless of the operator's own
      ~/.claude/settings.json. (`bypassPermissionsModeAccepted` does NOT gate
      this disclaimer — verified by live test.)

    Compact JSON (no spaces) so it survives as a single argv token. Additive:
    merges on top of the burst home's ~/.claude/settings.json.
    """
    return ["--settings", json.dumps(
        {"prefersReducedMotion": True, "skipDangerousModePermissionPrompt": True},
        separators=(",", ":"),
    )]


_GEMINI_ACCESSIBILITY_HELPER = r"""
import json, os, sys, uuid
from pathlib import Path
home = Path(os.environ.get("HOME") or Path.home())
path = home / ".gemini" / "settings.json"
path.parent.mkdir(parents=True, exist_ok=True)
enable_screen_reader = sys.argv[1] == "1"
disable_loading_phrases = sys.argv[2] == "1"
disable_spinner = sys.argv[3] == "1"
try:
    current = json.loads(path.read_text(encoding="utf-8")) if path.is_file() else {}
except Exception:
    current = {}
if not isinstance(current, dict):
    current = {}
ui = current.get("ui") if isinstance(current.get("ui"), dict) else {}
current["ui"] = ui
acc = ui.get("accessibility") if isinstance(ui.get("accessibility"), dict) else {}
ui["accessibility"] = acc
before = json.dumps(current, sort_keys=True)
if enable_screen_reader:
    acc["screenReader"] = True
if disable_loading_phrases:
    acc["enableLoadingPhrases"] = False
    ui["loadingPhrases"] = "off"
if disable_spinner:
    ui["showSpinner"] = False
after = json.dumps(current, sort_keys=True)
if before != after:
    tmp = path.with_suffix(f".json.tmp.{os.getpid()}.{uuid.uuid4().hex[:6]}")
    tmp.write_text(json.dumps(current, indent=2) + "\n", encoding="utf-8")
    try:
        tmp.chmod(0o600)
    except OSError:
        pass
    try:
        os.replace(tmp, path)
    except OSError:
        tmp.unlink(missing_ok=True)
"""


def ensure_gemini_accessibility_settings(
    *,
    enable_screen_reader: bool = True,
    disable_loading_phrases: bool = True,
    disable_spinner: bool = True,
    burst_home: Optional[Path] = None,
) -> None:
    """Merge accessibility keys into the target user's ~/.gemini/settings.json.

    Post-bwrap-only: writes directly under `burst_home` (or supervisor's
    HOME if unset). The gemini child runs under the same uid as the
    supervisor inside bwrap, with HOME set to `burst_home`.
    """
    path = (burst_home or Path.home()) / ".gemini" / "settings.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    with _GEMINI_SETTINGS_LOCK:
        try:
            current = json.loads(path.read_text(encoding="utf-8")) if path.is_file() else {}
        except Exception:
            current = {}
        if not isinstance(current, dict):
            current = {}
        ui = current.get("ui") if isinstance(current.get("ui"), dict) else {}
        current["ui"] = ui
        acc = ui.get("accessibility") if isinstance(ui.get("accessibility"), dict) else {}
        ui["accessibility"] = acc
        before = json.dumps(current, sort_keys=True)
        if enable_screen_reader:
            acc["screenReader"] = True
        if disable_loading_phrases:
            acc["enableLoadingPhrases"] = False
            ui["loadingPhrases"] = "off"
        if disable_spinner:
            ui["showSpinner"] = False
        after = json.dumps(current, sort_keys=True)
        if before != after:
            tmp = path.with_suffix(f".json.tmp.{os.getpid()}.{uuid.uuid4().hex[:6]}")
            tmp.write_text(json.dumps(current, indent=2) + "\n", encoding="utf-8")
            try:
                tmp.chmod(0o600)
            except OSError:
                pass
            try:
                os.replace(tmp, path)
            except OSError:
                tmp.unlink(missing_ok=True)


def claude_project_slug(work_dir: Path) -> str:
    """Claude normalizes paths to slugs in ~/.claude/projects/."""
    return re.sub(r"[^A-Za-z0-9_-]", "-", str(work_dir))


def claude_transcript_path(work_dir: Path, session_id: str, *, home: Optional[Path] = None) -> Path:
    root = (home or Path.home()) / ".claude" / "projects"
    return root / claude_project_slug(work_dir) / f"{session_id}.jsonl"


def claude_session_exists(
    work_dir: Path,
    session_id: str,
    *,
    home: Optional[Path] = None,
) -> bool:
    """Return True if claude's session transcript already exists on disk.

    Post-bwrap-only: supervisor reads directly (no sudo wrap).
    """
    path = claude_transcript_path(work_dir, session_id, home=home)
    return path.is_file()


def _stat_mtime_ns_maybe_sudo(
    path: Path,
) -> int:
    """Return mtime_ns of `path`, 0 if missing or inaccessible.

    Post-bwrap-only: supervisor reads directly (no sudo). Name kept for
    callers; the `maybe_sudo` suffix is legacy.
    """
    try:
        return int(path.stat().st_mtime_ns)
    except (FileNotFoundError, PermissionError, OSError):
        return 0


def _latest_gemini_chat_mtime_ns_maybe_sudo(
    chats_dir: Path,
) -> int:
    """Return the highest mtime_ns across session-*.json files. Zero if none /
    inaccessible. Post-bwrap-only: direct stat (no sudo)."""
    try:
        if not chats_dir.is_dir():
            return 0
        best = 0
        for p in chats_dir.glob("session-*.json"):
            try:
                m = int(p.stat().st_mtime_ns)
            except OSError:
                continue
            if m > best:
                best = m
        return best
    except (PermissionError, OSError):
        return 0


def agent_session_transcript_mtime_ns(
    provider: str,
    *,
    cwd: Path,
    session_id: Optional[str] = None,
    burst_home: Optional[Path] = None,
) -> int:
    """Positive work-signal probe: latest mtime_ns of the agent's session
    transcript file(s).

    Claude writes to `~/.claude/projects/<slug>/<session_id>.jsonl`, appending
    a record per assistant turn / tool_use / thinking block. Gemini writes to
    `~/.gemini/tmp/<lowercased-projdir>/chats/session-*.json`, rewriting per
    message. In both cases, the file's mtime advances only when the agent
    process performs an actual write — a TUI-render thread cannot forge it
    without syscall activity. This makes it the strongest liveness signal
    for apparent_stall / inactivity detection.

    Returns 0 when: no session_id for claude / chats dir missing for gemini /
    any stat error.
    """
    home = burst_home or Path.home()
    if provider == "claude":
        if not session_id:
            return 0
        path = claude_transcript_path(cwd, session_id, home=home)
        return _stat_mtime_ns_maybe_sudo(path)
    if provider == "gemini":
        chats = gemini_chats_dir(cwd, home=home)
        return _latest_gemini_chat_mtime_ns_maybe_sudo(chats)
    return 0


def _read_text_maybe_sudo(
    path: Path,
) -> Optional[str]:
    """Read a file's full text.

    Post-bwrap-only: direct read (no sudo). Name kept for callers.
    """
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except (PermissionError, FileNotFoundError):
        return None
    except Exception:
        return None


def claude_last_assistant_message(
    work_dir: Path,
    session_id: str,
    *,
    home: Optional[Path] = None,
) -> str:
    """Return the content of the most recent assistant turn from claude's transcript."""
    path = claude_transcript_path(work_dir, session_id, home=home)
    text = _read_text_maybe_sudo(path)
    if text is None:
        return ""
    last_text = ""
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("type") != "assistant":
            continue
        msg = rec.get("message") or {}
        if not isinstance(msg, dict):
            continue
        parts = msg.get("content") or []
        texts: List[str] = []
        if isinstance(parts, list):
            for part in parts:
                if isinstance(part, dict) and part.get("type") == "text":
                    txt = str(part.get("text", ""))
                    if txt:
                        texts.append(txt)
        elif isinstance(parts, str):
            texts.append(parts)
        if texts:
            last_text = "\n".join(texts)
    return last_text


def gemini_project_dirname(work_dir: Path) -> str:
    # Gemini CLI normalizes the project tmp-dir name to lowercase. Match that
    # exactly — otherwise transcript lookups miss files that gemini wrote under
    # the lowercased path (observed e.g. cwd `trial-B` → gemini stores at
    # `~/.gemini/tmp/trial-b/`).
    return work_dir.resolve().name.lower()


def gemini_chats_dir(work_dir: Path, *, home: Optional[Path] = None) -> Path:
    root = (home or Path.home()) / ".gemini" / "tmp"
    return root / gemini_project_dirname(work_dir) / "chats"


def gemini_find_session_chat(work_dir: Path, session_id: str, *, home: Optional[Path] = None) -> Optional[Path]:
    """Find the gemini chat JSON file for a given session_id."""
    chats = gemini_chats_dir(work_dir, home=home)
    if not chats.is_dir():
        return None
    # file naming: session-<ISO>-<sid-prefix>.json
    sid_prefix = session_id.split("-")[0]
    for path in sorted(chats.glob("session-*.json")):
        if sid_prefix in path.name:
            return path
    return None


def gemini_latest_session_chat(
    work_dir: Path,
    *,
    home: Optional[Path] = None,
) -> Optional[Path]:
    """Return the most recent gemini session-*.json under `home`. None if none.

    Post-bwrap-only: direct read (no sudo wrap).
    """
    chats = gemini_chats_dir(work_dir, home=home)
    if not chats.is_dir():
        return None
    paths = sorted(chats.glob("session-*.json"), key=lambda p: p.stat().st_mtime_ns)
    return paths[-1] if paths else None


_GEMINI_TRANSCRIPT_READER = r"""
import json, os, sys
from pathlib import Path
home = Path(os.environ.get("HOME") or Path.home())
work_dir_name = sys.argv[1]
session_id = sys.argv[2] if len(sys.argv) > 2 else ""
chats_dir = home / ".gemini" / "tmp" / work_dir_name / "chats"
out = {"last_assistant": "", "usage": None, "path": ""}
if not chats_dir.is_dir():
    print(json.dumps(out)); sys.exit(0)
if session_id:
    sid_prefix = session_id.split("-")[0]
    candidates = [p for p in sorted(chats_dir.glob("session-*.json")) if sid_prefix in p.name]
    path = candidates[0] if candidates else None
else:
    paths = sorted(chats_dir.glob("session-*.json"), key=lambda p: p.stat().st_mtime_ns)
    path = paths[-1] if paths else None
if path is None or not path.is_file():
    print(json.dumps(out)); sys.exit(0)
out["path"] = str(path)
try:
    data = json.loads(path.read_text(encoding="utf-8"))
except Exception:
    print(json.dumps(out)); sys.exit(0)
msgs = data.get("messages", []) if isinstance(data, dict) else []
last_text = ""
agg = {"input": 0, "output": 0, "cached": 0, "thoughts": 0, "tool": 0, "total": 0}
model = ""
found = False
for m in msgs:
    if not isinstance(m, dict) or m.get("type") != "gemini":
        continue
    content = m.get("content") or ""
    if isinstance(content, str) and content.strip():
        last_text = content
    tokens = m.get("tokens") or {}
    if isinstance(tokens, dict):
        for k in list(agg.keys()):
            v = tokens.get(k)
            if isinstance(v, (int, float)):
                agg[k] += int(v); found = True
        if not model and isinstance(m.get("model"), str):
            model = m["model"]
out["last_assistant"] = last_text
if found:
    agg["model"] = model
    out["usage"] = agg
print(json.dumps(out))
"""


def _gemini_read_transcript(
    work_dir: Path,
    session_id: Optional[str],
    *,
    home: Optional[Path],
) -> dict:
    """Returns {"last_assistant": str, "usage": dict|None, "path": str}.

    Post-bwrap-only: direct in-process reads (no sudo).
    """
    path = (gemini_find_session_chat(work_dir, session_id, home=home)
            if session_id else gemini_latest_session_chat(work_dir, home=home))
    if path is None or not path.is_file():
        return {"last_assistant": "", "usage": None, "path": ""}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return {"last_assistant": "", "usage": None, "path": str(path)}
    msgs = data.get("messages", []) if isinstance(data, dict) else []
    last_text = ""
    agg = {"input": 0, "output": 0, "cached": 0, "thoughts": 0, "tool": 0, "total": 0}
    model = ""
    found = False
    for m in msgs:
        if not isinstance(m, dict) or m.get("type") != "gemini":
            continue
        content = m.get("content") or ""
        if isinstance(content, str) and content.strip():
            last_text = content
        tokens = m.get("tokens") or {}
        if isinstance(tokens, dict):
            for k in list(agg.keys()):
                v = tokens.get(k)
                if isinstance(v, (int, float)):
                    agg[k] += int(v); found = True
            if not model and isinstance(m.get("model"), str):
                model = m["model"]
    usage = None
    if found:
        agg["model"] = model
        usage = agg
    return {"last_assistant": last_text, "usage": usage, "path": str(path)}


def gemini_last_assistant_message(
    work_dir: Path,
    session_id: Optional[str] = None,
    *,
    home: Optional[Path] = None,
) -> str:
    """Return content of the last gemini-type message from the chat json."""
    return _gemini_read_transcript(
        work_dir, session_id, home=home
    )["last_assistant"]


def gemini_transcript_usage(
    work_dir: Path,
    session_id: Optional[str] = None,
    *,
    home: Optional[Path] = None,
) -> Optional[dict]:
    """Sum gemini-turn tokens across the session chat JSON."""
    return _gemini_read_transcript(
        work_dir, session_id, home=home
    )["usage"]


def claude_transcript_usage(
    work_dir: Path,
    session_id: str,
    *,
    home: Optional[Path] = None,
) -> Optional[dict]:
    """Sum usage across all assistant records (input+output+cache tokens) if present."""
    path = claude_transcript_path(work_dir, session_id, home=home)
    text = _read_text_maybe_sudo(path)
    if text is None:
        return None
    agg = {"input_tokens": 0, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
    model = ""
    any_found = False
    for line in text.splitlines():
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("type") != "assistant":
            continue
        msg = rec.get("message") or {}
        usage = msg.get("usage") if isinstance(msg, dict) else None
        if isinstance(usage, dict):
            any_found = True
            for k in list(agg.keys()):
                v = usage.get(k)
                if isinstance(v, int):
                    agg[k] += v
            if not model:
                model = str(msg.get("model", "") or "")
    if not any_found:
        return None
    agg["model"] = model
    agg["total_tokens"] = agg["input_tokens"] + agg["output_tokens"]
    return agg


_PRE_TRUST_HELPER = r"""
import json, os, sys, tempfile, uuid
from pathlib import Path

home = Path(os.environ.get("HOME") or Path.home())
trust_path = home / ".gemini" / "trustedFolders.json"
trust_path.parent.mkdir(parents=True, exist_ok=True)
key = sys.argv[1]
try:
    data = json.loads(trust_path.read_text(encoding="utf-8"))
except Exception:
    data = {}
if not isinstance(data, dict):
    data = {}
if data.get(key) == "TRUST_FOLDER":
    sys.exit(0)
data[key] = "TRUST_FOLDER"
tmp = trust_path.with_suffix(f".json.tmp.{os.getpid()}.{uuid.uuid4().hex[:6]}")
tmp.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
try:
    tmp.chmod(0o600)
except OSError:
    pass
try:
    os.replace(tmp, trust_path)
except OSError:
    tmp.unlink(missing_ok=True)
"""


def pre_trust_gemini_folder(
    cwd: Path,
    *,
    burst_home: Optional[Path] = None,
) -> None:
    """Seed `<burst_home>/.gemini/trustedFolders.json` so startup skips the trust dialog.

    Post-bwrap-only: writes directly under `burst_home` (or caller's own
    HOME if unset). The gemini child runs under the same uid as the
    supervisor inside bwrap, with HOME set to `burst_home`.

    Uses a module-level lock + per-lane tmpfile to avoid concurrent launches
    racing on the same .tmp path.
    """
    key = str(cwd.resolve())
    trust_path = (burst_home or Path.home()) / ".gemini" / "trustedFolders.json"
    trust_path.parent.mkdir(parents=True, exist_ok=True)
    with _GEMINI_SETTINGS_LOCK:
        try:
            data = json.loads(trust_path.read_text(encoding="utf-8"))
        except Exception:
            data = {}
        if not isinstance(data, dict):
            data = {}
        if data.get(key) == "TRUST_FOLDER":
            return
        data[key] = "TRUST_FOLDER"
        tmp = trust_path.with_suffix(f".json.tmp.{os.getpid()}.{uuid.uuid4().hex[:6]}")
        tmp.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
        try:
            tmp.chmod(0o600)
        except OSError:
            pass
        try:
            os.replace(tmp, trust_path)
        except OSError:
            tmp.unlink(missing_ok=True)


def launch_gemini(
    cwd: Path,
    *,
    model: Optional[str] = None,
    name_hint: str = "gemini-test",
    extra_args: Optional[List[str]] = None,
    screen_reader: bool = False,
    yolo: bool = True,
) -> AgentHandle:
    cwd.mkdir(parents=True, exist_ok=True)
    pre_trust_gemini_folder(cwd)
    session = f"trellis-{name_hint}"
    cmd = ["gemini"]
    if yolo:
        cmd.append("--yolo")
    if screen_reader:
        cmd.append("--screen-reader")
    if model and model != "gemini-auto":
        cmd.extend(["--model", model])
    if extra_args:
        cmd.extend(extra_args)
    kill_session(session)
    new_session(session, cwd=cwd, cmd=cmd)
    return AgentHandle(session=session, cwd=cwd, provider="gemini")


# ---- startup dialog handling -----------------------------------------------

# (dialog-text-pattern, keys-to-send-to-dismiss)
_CLAUDE_DIALOGS = [
    ("Is this a project you created", ("Enter",)),
    ("Yes, I trust this folder", ("Enter",)),
    ("Do you trust the files in this folder", ("Enter",)),
    ("Accept Terms", ("Enter",)),
    ("Use the arrow keys", ("Enter",)),
    # Long-lived sticky sessions (100k+ tokens, 1h+ age) trigger this
    # interactive prompt on --resume. Default-highlighted option is
    # "Resume from summary"; Enter accepts it. Without this, the burst
    # hangs at the prompt forever — no settle, no done marker. Observed
    # on reviewer 149 after several hours of reuse of the sticky
    # reviewer-review session.
    ("Resume from summary (recommended)", ("Enter",)),
    # Safety net in case claude rewords the recommended option but
    # keeps the preamble.
    ("We recommend resuming from a summary", ("Enter",)),
]
_GEMINI_DIALOGS = [
    ("Do you trust the files in this folder", ("Enter",)),
    ("Trust folder (", ("Enter",)),
    ("Select Theme", ("Enter",)),
    ("? Select a theme", ("Enter",)),
    ("Log in with Google", ("Enter",)),  # shouldn't trigger since we're OAuth'd
]


def _dialogs_for(provider: str):
    return {"claude": _CLAUDE_DIALOGS, "gemini": _GEMINI_DIALOGS}.get(provider, [])


def _detect_dialog(provider: str, norm_screen: str) -> Optional[Tuple[str, Tuple[str, ...]]]:
    for needle, keys in _dialogs_for(provider):
        if needle in norm_screen:
            return needle, keys
    return None


# ---- stale-process cleanup for session-id conflicts ------------------------

_CLAUDE_SESSION_CONFLICT_MARKERS = (
    "already in use",
    "Session ID ",
    "another Claude process is using",
)


def _claude_session_id_in_use(screen: str) -> bool:
    t = strip_ansi(screen)
    return any(m in t for m in _CLAUDE_SESSION_CONFLICT_MARKERS) and "Session ID" in t


def _pids_using_claude_session_id(session_id: str) -> List[int]:
    """Find any live processes whose cmdline contains the session_id.

    We scan /proc/*/cmdline — cheap and independent of ps formatting. This
    catches stray claude processes that would collide with a --resume.
    """
    found: list[int] = []
    sid = str(session_id or "").strip()
    if not sid:
        return found
    try:
        for pid_dir in Path("/proc").iterdir():
            name = pid_dir.name
            if not name.isdigit():
                continue
            try:
                cmdline = (pid_dir / "cmdline").read_bytes()
            except OSError:
                continue
            if sid.encode() in cmdline and (b"claude" in cmdline or b"gemini" in cmdline):
                found.append(int(name))
    except OSError:
        pass
    return found


def kill_conflicting_sessions(session_id: str) -> List[int]:
    """SIGTERM + SIGKILL fallback for any processes holding the session_id."""
    killed: list[int] = []
    for pid in _pids_using_claude_session_id(session_id):
        try:
            os.kill(pid, signal.SIGTERM)
            killed.append(pid)
        except Exception:
            continue
    if killed:
        time.sleep(1)
        for pid in list(killed):
            try:
                os.kill(pid, 0)  # still alive?
            except OSError:
                continue
            try:
                os.kill(pid, signal.SIGKILL)
            except Exception:
                pass
    return killed


# ---- post-startup liveness check -------------------------------------------

def post_startup_liveness(h: AgentHandle, *, grace_seconds: float = 3.0) -> Tuple[bool, str]:
    """After settle, confirm the agent process is alive and the pane isn't in a
    degenerate 'empty, no input box' state.

    Returns (alive, reason). alive=False means we should NOT send a prompt.
    """
    if pane_dead(h.session):
        return False, "pane_dead"
    pid = pane_pid(h.session)
    if pid is None:
        return False, "no_pid"
    # Check process is actually alive.
    try:
        os.kill(pid, 0)
    except OSError:
        return False, "pid_gone"
    # Quick screen sanity: has any content rendered?
    screen = capture(h.session, history=False)
    norm = normalize_pane(screen)
    if not norm.strip():
        return False, "empty_pane"
    if agent_requires_auth(screen, h.provider):
        return False, "auth_expired"
    return True, "live"


_GEMINI_RESTART_RE = re.compile(r"Gemini CLI is restarting", re.IGNORECASE)


def _gemini_is_restarting(screen: str) -> bool:
    return bool(_GEMINI_RESTART_RE.search(strip_ansi(screen)))


def _note_auth_required_if_seen(h: AgentHandle, seen: List[str]) -> bool:
    try:
        screen = capture(h.session, history=False)
    except subprocess.CalledProcessError:
        return False
    if agent_requires_auth(screen, h.provider):
        seen.append(f"{h.provider}:auth_expired")
        return True
    return False


def settle_until_ready(
    h: AgentHandle,
    *,
    # Wall-clock timeout for STARTUP only (TUI render + OAuth + dialog dismissal).
    # 3 min is generous — typical cold start is <15s, but OAuth refresh or
    # slow login on first use can add time.
    total_timeout: float = 180.0,
    poll_interval: float = 0.5,
) -> Tuple[bool, List[str]]:
    """Combined dialog dismissal + readiness wait.

    Loop: capture pane; if a known dialog is detected, send its dismiss keys
    and wait briefly; else if the empty-input-box heuristic is true for two
    consecutive polls, return ready. Times out as a whole.
    """
    seen: List[str] = []
    deadline = time.monotonic() + total_timeout
    consecutive_ready = 0
    last_dialog_at = 0.0
    while time.monotonic() < deadline:
        if pane_dead(h.session):
            return False, seen + ["pane_dead"]
        try:
            screen = capture(h.session, history=False)
        except subprocess.CalledProcessError:
            time.sleep(poll_interval)
            continue
        # Early exit on auth-expired — caller must surface this, not retry.
        if agent_requires_auth(screen, h.provider):
            return False, seen + [f"{h.provider}:auth_expired"]
        norm = normalize_pane(screen)
        # Gemini: if the CLI is mid-restart (e.g., after trust-folder change),
        # don't declare ready even if the pre-restart pane shows a placeholder.
        # Wait for the "restarting" banner to clear.
        if h.provider == "gemini" and _gemini_is_restarting(screen):
            if "gemini:restarting" not in seen:
                seen.append("gemini:restarting")
            consecutive_ready = 0
            time.sleep(poll_interval)
            continue
        dialog = _detect_dialog(h.provider, norm)
        if dialog is not None and (time.monotonic() - last_dialog_at) > 1.0:
            needle, keys = dialog
            for k in keys:
                tmux("send-keys", "-t", h.session, k, check=True, timeout=5)
            seen.append(f"{h.provider}:{needle[:30]!r}")
            last_dialog_at = time.monotonic()
            consecutive_ready = 0
            time.sleep(1.0)
            continue
        if h.provider == "claude":
            box_empty = _claude_input_line_is_empty(norm)
        elif h.provider == "gemini":
            box_empty = _gemini_input_line_is_empty(norm)
        else:
            box_empty = False
        if box_empty:
            consecutive_ready += 1
            if consecutive_ready >= 2:
                return True, seen
        else:
            consecutive_ready = 0
        time.sleep(poll_interval)
    return False, seen + ["settle_timeout"]


def send_prompt(
    h: AgentHandle,
    prompt: str,
    *,
    large_threshold: int = 4096,
    pre_enter_settle: float = 2.0,
    enter_verify_timeout: float = 8.0,
    enter_max_retries: int = 3,
) -> None:
    """Send a prompt via tmux send-keys, falling back to load-buffer for large prompts.

    For large pasted prompts, gemini's TUI can take a few seconds to finish
    rendering the paste before it accepts further keypresses. If we send
    Enter too early, the keypress gets eaten by the paste-rendering pipeline
    and the prompt sits unsubmitted in the input box (worker idles forever
    while the supervisor's `wait_until_idle` waits for a busy marker that
    never appears). Mitigate with two layers:

      (1) `pre_enter_settle` — sleep this long after the paste finishes,
          giving the TUI time to drain the paste buffer (default 2.0s).
      (2) After sending Enter, verify a busy marker appears within
          `enter_verify_timeout` (default 8.0s). If not, re-send Enter up to
          `enter_max_retries` times. Sending Enter while the agent is
          actually busy is safe — its TUI ignores or queues input until
          ready — so the worst case is a no-op extra keypress.
    """
    if len(prompt.encode("utf-8")) <= large_threshold:
        try:
            tmux("send-keys", "-t", h.session, "-l", prompt, check=True, timeout=30)
        except Exception:
            _send_via_buffer(h.session, prompt)
    else:
        _send_via_buffer(h.session, prompt)
    time.sleep(pre_enter_settle)
    for attempt in range(enter_max_retries):
        tmux("send-keys", "-t", h.session, "Enter", check=True, timeout=5)
        deadline = time.monotonic() + enter_verify_timeout
        while time.monotonic() < deadline:
            screen = capture(h.session, history=False)
            if agent_is_busy(screen, h.provider):
                return
            time.sleep(0.5)
        # Not busy yet — Enter may have been swallowed. Retry.


def _send_via_buffer(session: str, text: str) -> None:
    """Paste text via tmux buffer.

    Uses tmux bracketed-paste mode (`-p`). Without it, the target TUI
    sees each '\\n' inside the pasted content as a separate Enter press,
    which for agent TUIs like gemini splits one prompt into many
    separate submissions (observed: a 28KB prompt with ~100 newlines
    became 100+ user turns in a single gemini session, flooding the
    context and causing "I will read X" tool-call loops). `-p` wraps
    the paste in the DEC PM sequences that tell the TUI "treat this
    as a single clipboard paste, not line-by-line input".
    """
    bufname = f"trellis-buf-{uuid.uuid4().hex[:8]}"
    subprocess.run(
        _tmux_argv("load-buffer", "-b", bufname, "-"),
        input=text, text=True, capture_output=True, check=True, timeout=30,
    )
    tmux("paste-buffer", "-p", "-b", bufname, "-t", session, check=True, timeout=10)
    tmux("delete-buffer", "-b", bufname, check=False, timeout=5)


# ---- auth / login detection ------------------------------------------------

_CLAUDE_AUTH_MARKERS = (
    "Please log in",
    "Please authenticate",
    "claude auth login",
    "OAuth token expired",
    "Authentication required",
    "Sign in to Claude",
)
_GEMINI_AUTH_MARKERS = (
    "Please log in to Gemini",
    "Google sign-in required",
    "authentication failed",
    "Re-authenticate",
)


def agent_requires_auth(screen: str, provider: str) -> bool:
    t = strip_ansi(screen)
    markers = _CLAUDE_AUTH_MARKERS if provider == "claude" else _GEMINI_AUTH_MARKERS
    return any(m in t for m in markers)


def confirm_prompt_delivery(
    h: AgentHandle,
    *,
    prompt_head: str,
    timeout: float = 10.0,
    poll_interval: float = 0.5,
) -> Tuple[bool, str]:
    """After send_prompt, confirm the TUI received it.

    Positive delivery signals:
      - first ~30 chars of the prompt appear on the pane (agent echoes input)
      - agent busy-marker is visible (Thinking / esc to interrupt)

    When neither of those fires within `timeout`, the caller previously
    re-sent the prompt unconditionally. That turned out to be wrong: many
    agents (gemini in particular) process a pasted prompt without echoing
    any of it to the visible pane, and their busy-marker only becomes
    visible after a ~1-2 s "digesting" phase that can miss a 10 s window.
    Re-sending in that case appends a DUPLICATE user turn to the session,
    confusing the agent.

    New policy: only report "no_signal" (which the caller treats as
    dropped prompt requiring re-send) if the input box is PROVABLY empty
    at timeout — i.e., the paste never landed. Otherwise return
    "post_paste_quiet": delivered, just not visibly echoed. The caller
    will then let `wait_until_idle` do its job instead of stuffing a
    second prompt.
    """
    head_clean = strip_ansi(prompt_head[:30]).strip()
    head_fragment = head_clean[:20] if head_clean else ""
    deadline = time.monotonic() + timeout
    last_norm = ""
    while time.monotonic() < deadline:
        screen = capture(h.session, history=False)
        norm = normalize_pane(screen)
        last_norm = norm
        if head_fragment and head_fragment in norm:
            return True, "echoed_head"
        if agent_is_busy(screen, h.provider):
            return True, "busy_marker"
        time.sleep(poll_interval)
    # At timeout: only declare "dropped" if the input box is still empty.
    if h.provider == "claude":
        box_empty = _claude_input_line_is_empty(last_norm)
    elif h.provider == "gemini":
        box_empty = _gemini_input_line_is_empty(last_norm)
    else:
        box_empty = False
    if box_empty:
        return False, "no_signal"
    # Something's in the input area but not echoed as user-turn — trust that
    # the paste landed and gemini is silently digesting.
    return True, "post_paste_quiet"


# ---- CLI for smoke-testing -------------------------------------------------

def cmd_hello_claude(args: argparse.Namespace) -> int:
    workdir = Path(args.workdir).resolve()
    session_id = str(uuid.uuid4())
    h = launch_claude(workdir, session_id=session_id, name_hint=args.name)
    print(f"[launch] session={h.session} session_id={session_id}", file=sys.stderr)
    ready, dialogs = settle_until_ready(h, total_timeout=600.0)
    print(f"[settle] ready={ready} dialogs={dialogs}", file=sys.stderr)
    Path("/tmp/tmux-agent-exp/logs").mkdir(parents=True, exist_ok=True)
    (Path("/tmp/tmux-agent-exp/logs") / f"{h.session}.ready.txt").write_text(
        h.snapshot(), encoding="utf-8"
    )
    if not ready:
        print("[warn] prompt box not detected — continuing anyway", file=sys.stderr)
    baseline = baseline_snapshot(h)
    send_prompt(h, args.prompt)
    print(f"[sent] prompt={args.prompt!r}", file=sys.stderr)
    done, reason = wait_until_idle(
        h,
        min_stable_seconds=args.stable_seconds,
        total_timeout=args.wait_timeout,
        baseline=baseline,
    )
    print(f"[idle] done={done} reason={reason}", file=sys.stderr)
    snap = h.snapshot()
    out_path = Path("/tmp/tmux-agent-exp/logs") / f"{h.session}.final.txt"
    out_path.write_text(snap, encoding="utf-8")
    print(f"[snapshot] saved to {out_path}", file=sys.stderr)
    if not args.keep:
        kill_session(h.session)
    else:
        print(f"[keep] tmux attach -t {h.session}", file=sys.stderr)
    return 0 if done else 2


def cmd_hello_gemini(args: argparse.Namespace) -> int:
    workdir = Path(args.workdir).resolve()
    h = launch_gemini(
        workdir,
        name_hint=args.name,
        screen_reader=args.screen_reader,
    )
    print(f"[launch] session={h.session}", file=sys.stderr)
    ready, dialogs = settle_until_ready(h, total_timeout=600.0)
    print(f"[settle] ready={ready} dialogs={dialogs}", file=sys.stderr)
    Path("/tmp/tmux-agent-exp/logs").mkdir(parents=True, exist_ok=True)
    (Path("/tmp/tmux-agent-exp/logs") / f"{h.session}.ready.txt").write_text(
        h.snapshot(), encoding="utf-8"
    )
    if not ready:
        print("[warn] prompt box not detected — continuing anyway", file=sys.stderr)
    baseline = baseline_snapshot(h)
    send_prompt(h, args.prompt)
    print(f"[sent] prompt={args.prompt!r}", file=sys.stderr)
    done, reason = wait_until_idle(
        h,
        min_stable_seconds=args.stable_seconds,
        total_timeout=args.wait_timeout,
        baseline=baseline,
    )
    print(f"[idle] done={done} reason={reason}", file=sys.stderr)
    snap = h.snapshot()
    out_path = Path("/tmp/tmux-agent-exp/logs") / f"{h.session}.final.txt"
    out_path.write_text(snap, encoding="utf-8")
    print(f"[snapshot] saved to {out_path}", file=sys.stderr)
    if not args.keep:
        kill_session(h.session)
    else:
        print(f"[keep] tmux attach -t {h.session}", file=sys.stderr)
    return 0 if done else 2


def run_one_prompt(
    h: AgentHandle,
    prompt: str,
    *,
    stable_seconds: float,
    total_timeout: float,
    require_change_first: bool = True,
) -> Tuple[bool, str, str]:
    """Send one prompt to an already-ready handle; return (done, reason, final_snapshot)."""
    baseline = baseline_snapshot(h)
    send_prompt(h, prompt)
    done, reason = wait_until_idle(
        h,
        min_stable_seconds=stable_seconds,
        total_timeout=total_timeout,
        baseline=baseline,
        require_change_first=require_change_first,
    )
    return done, reason, h.snapshot()


def cmd_multiturn_claude(args: argparse.Namespace) -> int:
    workdir = Path(args.workdir).resolve()
    session_id = str(uuid.uuid4())
    h = launch_claude(workdir, session_id=session_id, name_hint=args.name)
    print(f"[launch] session={h.session} session_id={session_id}", file=sys.stderr)
    ready, dialogs = settle_until_ready(h, total_timeout=600.0)
    if dialogs:
        print(f"[settle] dialogs={dialogs}", file=sys.stderr)
    if not ready:
        print("[warn] initial readiness not detected", file=sys.stderr)
    Path("/tmp/tmux-agent-exp/logs").mkdir(parents=True, exist_ok=True)
    results = []
    prompts = [
        "Reply with exactly: HELLO-1",
        "Now reply with exactly: HELLO-2 — and include the fact I previously said HELLO-1 somewhere in brackets.",
        "Finally reply with exactly: HELLO-3 — include both prior phrases in brackets.",
    ]
    for i, prompt in enumerate(prompts, start=1):
        print(f"[turn {i}] sending prompt", file=sys.stderr)
        done, reason, snap = run_one_prompt(
            h, prompt, stable_seconds=args.stable_seconds, total_timeout=args.wait_timeout,
        )
        print(f"[turn {i}] done={done} reason={reason}", file=sys.stderr)
        results.append({"turn": i, "done": done, "reason": reason})
        (Path("/tmp/tmux-agent-exp/logs") / f"{h.session}.turn{i}.txt").write_text(snap, encoding="utf-8")
        if not done:
            break
        # Between turns we wait for the input box to return to empty.
        if not wait_until_prompt_ready(h, timeout=15.0):
            print(f"[turn {i}] warning: input box did not return to empty", file=sys.stderr)
    print(json.dumps(results, indent=2))
    if not args.keep:
        kill_session(h.session)
    return 0 if all(r["done"] for r in results) else 2


def gemini_delete_session(cwd: Path, index: int, *, timeout: float = 30.0) -> Tuple[bool, str]:
    """`gemini --delete-session <index>` in the given cwd."""
    proc = subprocess.run(
        ["gemini", "--delete-session", str(index)],
        cwd=str(cwd), capture_output=True, text=True, timeout=timeout, check=False,
    )
    return proc.returncode == 0, (proc.stdout + proc.stderr).strip()


def gemini_parsed_sessions(cwd: Path) -> List[dict]:
    """Parse `gemini --list-sessions` into [{index, title, sid, age}] entries."""
    raw = gemini_list_sessions(cwd)
    out: List[dict] = []
    for line in raw:
        # Example: "  1. Some prompt text (29 minutes ago) [6def8506-207e-477b-a419-e114a26aaa0a]"
        m = re.match(r"\s*(\d+)\.\s+(.*?)\s+\(([^)]+)\)\s+\[([0-9a-f-]+)\]\s*$", line)
        if m:
            out.append({
                "index": int(m.group(1)),
                "title": m.group(2),
                "age": m.group(3),
                "sid": m.group(4),
            })
    return out


def gemini_prune_sessions(
    cwd: Path, *, keep_latest: int = 5, timeout_per: float = 15.0,
) -> List[int]:
    """Delete all but the most-recent `keep_latest` sessions for this cwd.

    Gemini lists sessions most-recent first. Returns the indices deleted.
    """
    sessions = gemini_parsed_sessions(cwd)
    to_delete = sessions[keep_latest:]
    # Delete higher indices first to avoid index renumbering.
    to_delete.sort(key=lambda s: s["index"], reverse=True)
    deleted: List[int] = []
    for s in to_delete:
        ok, _ = gemini_delete_session(cwd, s["index"], timeout=timeout_per)
        if ok:
            deleted.append(s["index"])
    return deleted


def gemini_list_sessions(cwd: Path) -> List[str]:
    """Call gemini --list-sessions in cwd; return lines as shown."""
    proc = subprocess.run(
        ["gemini", "--list-sessions"],
        cwd=str(cwd),
        capture_output=True,
        text=True,
        timeout=30,
    )
    return [ln.strip() for ln in proc.stdout.splitlines() if ln.strip()]


def _evidence_snapshot(session_tmux: Optional[str], extra_text: str = "", max_chars: int = 2000) -> dict:
    """Collect a snapshot of pane content + metadata for burst_failed events."""
    pane = ""
    if session_tmux:
        try:
            pane = capture(session_tmux, history=True) or ""
        except Exception:
            pane = ""
    # Keep the tail; failures usually surface at the end.
    tail = pane[-max_chars:] if pane else ""
    return {
        "pane_tail": tail,
        "extra_text": extra_text[-max_chars:] if extra_text else "",
        "pane_chars_total": len(pane),
    }


def _emit_burst_launched(
    *, provider: str, role: str, scope: str, session_id: Optional[str],
    model: Optional[str], effort: Optional[str], account: Optional[str],
    sandbox: bool, prompt_bytes: int,
) -> None:
    emit_event("burst_launched", provider=provider, role=role, scope=scope,
               detail={
                   "session_id": session_id, "model": model, "effort": effort,
                   "account": account, "sandbox": sandbox,
                   "prompt_bytes": prompt_bytes,
               })


def _emit_burst_succeeded(
    *, provider: str, role: str, scope: str, session_id: Optional[str],
    model: Optional[str], account: Optional[str],
    reason: str, duration: float, cost_usd: Optional[float],
    transcript_len: int, attempts: int,
) -> None:
    emit_event("burst_succeeded", provider=provider, role=role, scope=scope,
               detail={
                   "session_id": session_id, "model": model, "account": account,
                   "reason": reason, "duration_seconds": round(duration, 2),
                   "cost_usd": cost_usd, "transcript_chars": transcript_len,
                   "attempts": attempts,
               })


def _emit_burst_failed(
    *, provider: str, role: str, scope: str, session_id: Optional[str],
    reason: str, duration: float, attempts: list, evidence: dict,
    account: Optional[str] = None, retryable: bool = True,
) -> None:
    emit_event("burst_failed", provider=provider, role=role, scope=scope,
               detail={
                   "session_id": session_id, "reason": reason,
                   "duration_seconds": round(duration, 2),
                   "attempts": attempts, "account": account,
                   "retryable": retryable, "evidence": evidence,
               })


def to_burst_result_dict(
    *,
    ok: bool,
    reason: str,
    started_at: float,
    captured_output: str,
    attempts: list,
    usage: Optional[dict],
    transcript_path: Optional[Path],
    session_id: Optional[str] = None,
    extra: Optional[dict] = None,
    provider: Optional[str] = None,
    burst_home: Optional[Path] = None,
) -> dict:
    """Return a dict matching trellis.adapters.BurstResult fields plus extras.

    The adapter layer at burst.py:241,326 can hand this straight to BurstResult(**{...}).
    """
    error = "" if ok else reason
    # GATE H: when a burst failed because its provider CLI wasn't on the
    # sandbox PATH, the captured pane snapshot shows the shell's
    # "command not found". Replace the opaque reason with an operator-facing
    # message naming the provider + the PATH searched. We key ONLY off the
    # captured-output text (not the bare `reason`) so unrelated pane deaths
    # (auth/rate-limit/crash) keep their real reason.
    if not ok and provider and captured_output:
        from trellis.host_runtime import provider_cli_not_found_detail
        not_found = provider_cli_not_found_detail(
            provider,
            exit_code=None,
            output=captured_output,
            burst_home=burst_home,
        )
        if not_found:
            error = not_found
    stall_recoveries = sum(1 for a in attempts if a.get("kind") == "resume" or a.get("live") is False)
    recovery_log = [f"attempt={a.get('attempt')} kind={a.get('kind')} reason={a.get('reason', 'settle')}"
                    for a in attempts]
    result = {
        # BurstResult fields — drop-in to adapters.BurstResult:
        "ok": ok,
        "exit_code": 0 if ok else None,
        "captured_output": captured_output,
        "duration_seconds": max(0.0, time.monotonic() - started_at),
        "stall_recoveries": stall_recoveries,
        "usage": usage,
        "error": error,
        "recovery_log": recovery_log,
        "transcript_path": transcript_path,
        # Extras the caller / bridge can rely on:
        "reason": reason,
        "attempts": attempts,
        "session_id": session_id,
    }
    if extra:
        result.update(extra)
    return result


def run_claude_burst(
    *,
    cwd: Path,
    prompt: str,
    session_id: Optional[str] = None,
    model: Optional[str] = None,
    effort: Optional[str] = None,
    role: str = "worker",
    session_scope: str = "",
    name_hint: Optional[str] = None,
    done_file: Optional[Path] = None,
    workspace_paths: Optional[Sequence[Path]] = None,
    apparent_stall_seconds: float = 0.0,
    # Stability detection threshold. Policy: ERR ON THE SIDE OF WAITING MUCH
    # LONGER. A pane going quiet for minutes between tool calls is normal for
    # hard reasoning. 20 min of true pane-silence before we call a burst done
    # is extreme but intentional — until we observe this causing a frequent
    # real-world cost, we'd rather wait 20 min on a done-but-silent burst
    # than kill one that was thinking. Bursts that write a `done_file`
    # terminate immediately (they don't pay this tail cost).
    stable_seconds: float = 1200.0,
    # INACTIVITY timeout passed to wait_until_idle. Resets on any progress
    # (pane diff, FS change, busy marker). 60 min of pure silence —
    # hard reasoning tasks can legitimately span that long.
    wait_timeout: float = 3600.0,
    max_restarts: int = 2,
    burst_home: Optional[Path] = None,
    sandbox_enabled: bool = False,
    sandbox_writable_paths: Optional[Sequence[Path]] = None,
    sandbox_config: Optional[SandboxConfig] = None,
    rate_limit_base_delay: float = 60.0,
    rate_limit_max_delay: float = 900.0,
    rate_limit_max_retries: int = 3,
    max_budget_usd: Optional[float] = None,
) -> dict:
    """High-level burst driver with restart-on-fault + exponential rate-limit backoff."""
    started_at = time.monotonic()
    started_wall_ms = int(time.time() * 1000)
    cwd = cwd.resolve()
    cwd.mkdir(parents=True, exist_ok=True)
    # Quota probe (start-of-burst). Forced (NOT cooldown-gated) and dispatched
    # onto a thread pool so it runs IN PARALLEL with the burst itself — its
    # 10-25s wall cost adds 0 to a 30-min worker burst. NEVER raises; quota
    # tracking is cosmetic and cannot break a run.
    pre_probe_future = _submit_probe_for_burst(
        "claude", cwd, burst_home=burst_home,
    )
    # --max-budget-usd equivalent: interactive claude doesn't accept this flag
    # (it's -p-only), so enforce at the ledger level: refuse to launch if the
    # accumulated cost on this cwd's ledger already meets/exceeds the cap.
    if max_budget_usd is not None:
        summary = summarize_cost_ledger(default_cost_ledger_path(cwd))
        spent = float(summary.get("grand", {}).get("cost_usd", 0.0) or 0.0)
        if spent >= max_budget_usd:
            emit_event("budget_exceeded", provider="claude", role=role, scope=session_scope,
                       detail={"spent_usd": spent, "cap_usd": max_budget_usd})
            _emit_burst_failed(
                provider="claude", role=role, scope=session_scope,
                session_id=None, reason="budget_exceeded",
                duration=time.monotonic() - started_at,
                attempts=[],
                evidence={"pane_tail": "", "extra_text": f"spent={spent} cap={max_budget_usd}", "pane_chars_total": 0},
                account=None, retryable=False,
            )
            return to_burst_result_dict(
                ok=False, reason="budget_exceeded", started_at=started_at,
                captured_output="", attempts=[], usage=None, transcript_path=None,
                session_id=None,
                extra={"spent_usd": spent, "max_budget_usd": max_budget_usd},
            )
    desired_identity = _session_identity_payload(
        provider="claude", model=model, effort=effort, session_scope=session_scope,
    )
    rate_limit_attempt_num = 0  # for exponential backoff
    # Identity drift: if sidecar says stored identity differs, force a fresh session_id
    # (don't resume onto the stale session with new flags unless kernel confirmed
    # those flags actually switch — which is true for model/effort, but not for
    # arbitrary extra_args, scope semantics etc).
    stored = load_session_identity(cwd, provider="claude", role=role, session_scope=session_scope)
    stored_sid = ""
    if stored:
        stored_sid = str(stored.get("session_id", "") or "")
        stored_identity = stored.get("identity") or {}
        if isinstance(stored_identity, dict) and not identities_match(stored_identity, desired_identity):
            # Identity mismatch → forcing fresh
            stored_sid = ""
    # If caller supplied a session_id, trust it (they know what they're doing);
    # else prefer the sidecar-stored one; else mint a fresh uuid.
    if session_id is None:
        session_id = stored_sid or str(uuid.uuid4())
    _emit_burst_launched(
        provider="claude", role=role, scope=session_scope, session_id=session_id,
        model=model, effort=effort, account=None,
        sandbox=sandbox_enabled,
        prompt_bytes=len(prompt.encode("utf-8")),
    )
    base_name = name_hint or session_name("claude", role, session_scope=session_scope)
    attempts = []
    rate_limit_retries_left = rate_limit_max_retries
    resume_known_bad = False  # set True after --resume fails; triggers fresh fallback
    last_snap = ""  # GATE H: last pane snapshot, for provider-CLI-not-found detection
    for attempt in range(max_restarts + 1):
        # Decide fresh vs resume.
        #   attempt 0: fresh (first launch) unless we had a good stored session and
        #              caller wants to resume — but we always start fresh with new sid.
        #   attempt N>0: resume, unless resume has been proven bad → fresh with NEW sid.
        if attempt == 0 or resume_known_bad:
            kind = "fresh"
            if resume_known_bad:
                session_id = str(uuid.uuid4())
                resume_known_bad = False
        else:
            kind = "resume"
        # If the session_id we're about to "freshly" launch actually has a
        # transcript already on disk (sticky session carried over from a
        # previous burst), use --resume instead. `--session-id <existing>`
        # makes claude error out with "Session ID can only be used with
        # --continue or --resume if --fork-session is also specified", which
        # manifests as pane_dead on attempt 0 and burns a restart slot.
        if kind == "fresh" and claude_session_exists(
            cwd, session_id, home=burst_home,
        ):
            kind = "resume"
        session_tmux = base_name + ("" if attempt == 0 else f"-r{attempt}")
        kill_session(session_tmux)
        if kind == "fresh":
            claude_argv = ["claude", "--dangerously-skip-permissions", "--session-id", session_id]
        else:
            claude_argv = ["claude", "--dangerously-skip-permissions", "--resume", session_id]
        if model:
            claude_argv.extend(["--model", model])
        if effort:
            claude_argv.extend(["--effort", effort])
        # Per-launch --settings: disable cycling animations (so a stuck TUI is
        # distinguishable from a live one; the "esc to interrupt" footer is
        # unaffected, so agent_is_busy still fires) and pre-accept the
        # bypass-permissions disclaimer so a fresh-machine claude doesn't block
        # on it. See claude_launch_settings_args() for details.
        claude_argv.extend(claude_launch_settings_args())
        # Phase 4 bwrap-only migration: wrap with bwrap directly; the
        # supervisor user launches tmux + bwrap as itself. Per-burst
        # $HOME isolation is enforced inside bwrap via `--setenv HOME
        # <burst_home>`; the per-burst fake-home is seeded by
        # `trellis.burst_home.seed_burst_home` before this point.
        wrapped = sandbox_wrap(
            claude_argv,
            enabled=sandbox_enabled,
            work_dir=cwd,
            burst_home=burst_home,
            writable_paths=sandbox_writable_paths,
            role=role,
            sandbox_config=sandbox_config,
        )
        # PATH is forwarded into bwrap via the parent env (bwrap inherits
        # the parent's env unless `--clearenv` is passed). The per-burst
        # PATH prepends `<burst_home>/.trellis-npm/bin` etc. so a per-user
        # claude install takes precedence over the system claude wrapper.
        home_for_path = burst_home or Path.home()
        wrapped = ["env", f"PATH={worker_path_env(home_for_path)}", *wrapped]
        # Claude's default resume thresholds (70min / 100k tokens) still
        # apply — when hit, the sticky session shows the
        # "Resume from summary (recommended) / full / never" dialog. We
        # rely on settle_until_ready's dialog handler to auto-press Enter,
        # which picks the highlighted default "compact" option, so the
        # session gets compacted at resume without user intervention.
        # (Env vars CLAUDE_CODE_RESUME_THRESHOLD_MINUTES and
        # CLAUDE_CODE_RESUME_TOKEN_THRESHOLD are available if thresholds
        # need tuning, but defaults are fine and compaction is the
        # behavior we want.)
        new_session(session_tmux, cwd=cwd, cmd=wrapped)
        emit_event("launch", session=session_tmux, provider="claude",
                   role=role, scope=session_scope, attempt=attempt,
                   detail={"kind": kind, "model": model, "effort": effort,
                           "session_id": session_id,
                           "sandbox": sandbox_enabled})
        h = AgentHandle(session=session_tmux, cwd=cwd, provider="claude", session_id=session_id)
        ready, dialogs = settle_until_ready(h, total_timeout=600.0)
        emit_event("settle", session=session_tmux, provider="claude",
                   role=role, scope=session_scope, attempt=attempt,
                   detail={"ready": ready, "dialogs": dialogs})
        if not ready:
            snap = h.snapshot()
            last_snap = snap or last_snap
            auth_expired = any("auth_expired" in d for d in dialogs)
            pane_was_dead = "pane_dead" in dialogs
            # Session-id conflict detection on fresh launches: kill offenders then retry.
            session_id_collision = (
                kind == "fresh" and _claude_session_id_in_use(snap)
            )
            kill_session(session_tmux)
            if session_id_collision:
                killed = kill_conflicting_sessions(session_id)
                attempts.append({
                    "attempt": attempt, "kind": kind, "settle": False, "dialogs": dialogs,
                    "rate_limited": is_rate_limited(snap),
                    "session_id_collision": True,
                    "killed_pids": killed,
                })
                emit_event("session_id_collision", session=session_tmux, provider="claude",
                           role=role, scope=session_scope, attempt=attempt,
                           detail={"session_id": session_id, "killed_pids": killed})
                # Don't burn a max_restarts slot on a pure collision — retry immediately.
                continue
            attempts.append({
                "attempt": attempt, "kind": kind, "settle": False, "dialogs": dialogs,
                "rate_limited": is_rate_limited(snap),
            })
            if auth_expired:
                _emit_burst_failed(
                    provider="claude", role=role, scope=session_scope,
                    session_id=session_id, reason="auth_expired",
                    duration=time.monotonic() - started_at,
                    attempts=attempts, evidence=_evidence_snapshot(session_tmux, snap),
                    account=None, retryable=False,
                )
                return to_burst_result_dict(
                    ok=False, reason="auth_expired", started_at=started_at,
                    captured_output=snap, attempts=attempts, usage=None,
                    transcript_path=None, session_id=session_id,
                )
            if kind == "resume":
                resume_known_bad = True
            if is_rate_limited(snap) and rate_limit_retries_left > 0:
                delay = exponential_backoff(rate_limit_base_delay, rate_limit_attempt_num, rate_limit_max_delay)
                rate_limit_attempt_num += 1
                rate_limit_retries_left -= 1
                emit_event("rate_limited", session=session_tmux, attempt=attempt,
                           detail={"sleep": round(delay, 1), "source": "settle"})
                time.sleep(delay)
            else:
                # Settle failure without rate-limit — emit so the retry is
                # visible in the event log (e.g., transient `pane_dead`
                # on attempt 0 before the `--resume` retry succeeds).
                emit_event("burst_retry", session=session_tmux, provider="claude",
                           role=role, scope=session_scope, attempt=attempt,
                           detail={"reason": "settle_failed",
                                   "dialogs": dialogs,
                                   "resume_known_bad": resume_known_bad})
            continue
        # Post-startup liveness.
        alive, live_reason = post_startup_liveness(h, grace_seconds=2.0)
        if not alive:
            attempts.append({
                "attempt": attempt, "kind": kind, "settle": True, "dialogs": dialogs,
                "live": False, "live_reason": live_reason,
            })
            kill_session(session_tmux)
            if live_reason == "auth_expired":
                _emit_burst_failed(
                    provider="claude", role=role, scope=session_scope,
                    session_id=session_id, reason="auth_expired",
                    duration=time.monotonic() - started_at,
                    attempts=attempts, evidence=_evidence_snapshot(session_tmux, ""),
                    account=None, retryable=False,
                )
                return to_burst_result_dict(
                    ok=False, reason="auth_expired", started_at=started_at,
                    captured_output="", attempts=attempts, usage=None,
                    transcript_path=None, session_id=session_id,
                )
            if kind == "resume":
                resume_known_bad = True
            emit_event("burst_retry", session=session_tmux, provider="claude",
                       role=role, scope=session_scope, attempt=attempt,
                       detail={"reason": "post_startup_liveness_failed",
                               "live_reason": live_reason,
                               "resume_known_bad": resume_known_bad})
            continue
        baseline = baseline_snapshot(h)
        send_prompt(h, prompt)
        # Confirm delivery: if the prompt never appears and the agent never
        # becomes busy within 10s, re-send once — handles send-keys races.
        delivered, delivery_reason = confirm_prompt_delivery(h, prompt_head=prompt, timeout=30.0)
        if not delivered:
            send_prompt(h, prompt)
            delivered, delivery_reason = confirm_prompt_delivery(h, prompt_head=prompt, timeout=30.0)
        done, reason = wait_until_idle(
            h,
            min_stable_seconds=stable_seconds,
            total_timeout=wait_timeout,
            baseline=baseline,
            done_file=done_file,
            workspace_paths=workspace_paths,
            liveness_probe=lambda: agent_session_transcript_mtime_ns(
                "claude", cwd=cwd, session_id=session_id,
                burst_home=burst_home,
            ),
            apparent_stall_seconds=apparent_stall_seconds,
        )
        captured = h.snapshot()
        transcript_msg = claude_last_assistant_message(cwd, session_id, home=burst_home)
        usage = claude_transcript_usage(cwd, session_id, home=burst_home)
        attempts.append({
            "attempt": attempt, "kind": kind, "settle": True, "dialogs": dialogs,
            "done": done, "reason": reason,
            "rate_limited": is_rate_limited(captured),
        })
        # Missing-artifact guard: wait_until_idle returning "stable_*" means
        # the agent went quiet with output but DIDN'T write the requested
        # done_file sentinel. The caller expects that artifact (raw.json),
        # so treat this as a timeout and retry with --resume. Without this
        # guard, the bridge sees ok=True but no raw.json on disk and crashes.
        # Observed live on Corr/96 v1 (stable_1200s_after_busy, no artifact
        # written, bridge died looking for the file).
        if done and reason != "done_file" and done_file is not None and not done_file.exists():
            if attempt < max_restarts:
                emit_event("burst_retry", session=session_tmux, provider="claude",
                           role=role, scope=session_scope, attempt=attempt,
                           detail={"reason": "stable_without_done_file",
                                   "stable_reason": reason,
                                   "source": "post_idle_missing_artifact"})
                kill_session(session_tmux)
                continue
            # Last attempt exhausted — escalate by recording the failure
            # instead of a false success, so the caller can surface it.
            done = False
            reason = f"missing_artifact_after_{reason}"
        if done:
            if not transcript_msg:
                for _ in range(8):
                    time.sleep(0.5)
                    transcript_msg = claude_last_assistant_message(cwd, session_id, home=burst_home)
                    if transcript_msg:
                        usage = claude_transcript_usage(cwd, session_id, home=burst_home)
                        break
            kill_session(session_tmux)
            store_session_identity(
                cwd, provider="claude", role=role, session_scope=session_scope,
                session_id=session_id, identity=desired_identity,
            )
            # Enrich usage with cost_usd (not present in interactive transcript).
            # `cost` here is the SESSION CUMULATIVE cost — claude_cost_usd sums
            # across every assistant record in the transcript, and a resumed
            # session's transcript grows across bursts. Pass it as cumulative
            # to the ledger, which will compute a correct per-burst delta
            # against the most recent prior entry for this session_id.
            enriched_usage = dict(usage) if isinstance(usage, dict) else None
            cumulative_cost = claude_cost_usd(enriched_usage) if enriched_usage else None
            if enriched_usage is not None and cumulative_cost is not None:
                enriched_usage["cost_usd"] = cumulative_cost
            turn_count = _count_claude_assistant_turns(
                cwd, session_id, home=burst_home,
            )
            sub_tags = _read_claude_subscription_tags(
                burst_home=burst_home,
            )
            ledger_account = sub_tags.get("account")
            ledger_extra: Dict[str, Any] = {}
            if sub_tags.get("auth_method"):
                ledger_extra["auth_method"] = sub_tags["auth_method"]
            # Bracket the burst with a post-probe (forced; runs concurrently
            # with the cost-ledger write below). Then collect both pre and
            # post payloads with short timeouts. Either may resolve to None;
            # that just means the row records `quota_*: None` and contributes
            # nothing to per-burst USD attribution.
            post_probe_future = _submit_probe_for_burst(
                "claude", cwd, burst_home=burst_home,
            )
            pre_payload = _await_with_short_timeout(pre_probe_future, 5.0)
            post_payload = _await_with_short_timeout(post_probe_future, 30.0)
            quota_pre = _project_quota_for_ledger(pre_payload, "claude")
            quota_post = _project_quota_for_ledger(post_payload, "claude")
            append_cost_ledger(
                cwd, provider="claude", role=role, scope=session_scope,
                model=(enriched_usage or {}).get("model", "") if isinstance(enriched_usage, dict) else "",
                usage=enriched_usage, cost_usd=None,
                cumulative_cost_usd=cumulative_cost,
                duration_seconds=time.monotonic() - started_at,
                attempts=len(attempts), ok=True, reason=reason,
                session_id=session_id,
                account=ledger_account,
                ts_start=time.time() - (time.monotonic() - started_at),
                message_count=turn_count,
                subscription_tier=sub_tags.get("subscription_tier"),
                rate_limit_tier=sub_tags.get("rate_limit_tier"),
                extra=ledger_extra or None,
                quota_pre=quota_pre,
                quota_post=quota_post,
            )
            write_chat_artifacts(
                cwd, role=role,
                artifact_prefix=session_scope or session_id[:8],
                prompt=prompt,
                final_pane_text=captured,
                transcript_last_assistant=transcript_msg,
                transcript_path=claude_transcript_path(cwd, session_id, home=burst_home),
                provider="claude",
                model=(enriched_usage or {}).get("model") if isinstance(enriched_usage, dict) else model,
                session_id=session_id,
                started_at_ms=started_wall_ms,
                ended_at_ms=int(time.time() * 1000),
                scope=session_scope,
                persistent_transcript_name="transcript.jsonl",
            )
            _emit_burst_succeeded(
                provider="claude", role=role, scope=session_scope,
                session_id=session_id,
                model=(enriched_usage or {}).get("model") if isinstance(enriched_usage, dict) else None,
                account=None, reason=reason,
                duration=time.monotonic() - started_at, cost_usd=cumulative_cost,
                transcript_len=len(transcript_msg or ""), attempts=len(attempts),
            )
            # Post-burst quota probe was already kicked off above and the row
            # was stamped with the bracketed pre/post payloads — no extra
            # _quota_probe_hook needed here.
            return to_burst_result_dict(
                ok=True, reason=reason, started_at=started_at,
                captured_output=captured, attempts=attempts, usage=enriched_usage,
                transcript_path=claude_transcript_path(cwd, session_id, home=burst_home),
                session_id=session_id,
                extra={
                    "transcript_last_assistant": transcript_msg,
                    "screen_last": captured.splitlines()[-10:],
                    "cost_usd": cumulative_cost,
                },
            )
        # After-prompt failure path.
        if reason == "pane_dead" and kind == "resume":
            resume_known_bad = True
        if is_rate_limited(captured) and rate_limit_retries_left > 0:
            delay = exponential_backoff(rate_limit_base_delay, rate_limit_attempt_num, rate_limit_max_delay)
            rate_limit_attempt_num += 1
            rate_limit_retries_left -= 1
            kill_session(session_tmux)
            emit_event("rate_limited", session=session_tmux, attempt=attempt,
                       detail={"sleep": round(delay, 1), "source": "post_idle"})
            time.sleep(delay)
            continue
        # Claude fall-through retry (mirror of gemini): wait_until_idle
        # returned done=False with a non-rate-limit reason. Emit so no
        # retry is silent.
        emit_event("burst_retry", session=session_tmux, provider="claude",
                   role=role, scope=session_scope, attempt=attempt,
                   detail={"reason": reason, "done": done, "source": "post_idle_fallthrough"})
        kill_session(session_tmux)
    final_reason = attempts[-1].get("reason", "no_settle") if attempts else "no_attempts"
    _emit_burst_failed(
        provider="claude", role=role, scope=session_scope,
        session_id=session_id, reason=final_reason,
        duration=time.monotonic() - started_at,
        attempts=attempts,
        evidence=_evidence_snapshot(None, ""),
        account=None, retryable=True,
    )
    # Failed-burst ledger row with bracketing probes. Per-burst USD
    # attribution wants a row even for failures — a failed burst can still
    # consume quota (e.g., partial work before the rate-limit error). The
    # row records ok=False, cost_usd=0, usage=None; pre/post probes attach
    # whatever quota delta the supervisor observed across the failed run.
    post_probe_future_fail = _submit_probe_for_burst(
        "claude", cwd, burst_home=burst_home,
    )
    pre_payload_fail = _await_with_short_timeout(pre_probe_future, 5.0)
    post_payload_fail = _await_with_short_timeout(post_probe_future_fail, 30.0)
    try:
        append_cost_ledger(
            cwd, provider="claude", role=role, scope=session_scope,
            model=model or "",
            usage=None, cost_usd=0.0,
            duration_seconds=time.monotonic() - started_at,
            attempts=len(attempts), ok=False, reason=final_reason,
            session_id=session_id,
            ts_start=time.time() - (time.monotonic() - started_at),
            quota_pre=_project_quota_for_ledger(pre_payload_fail, "claude"),
            quota_post=_project_quota_for_ledger(post_payload_fail, "claude"),
        )
    except Exception:
        pass  # ledger writes are best-effort; never block return
    return to_burst_result_dict(
        ok=False, reason=final_reason, started_at=started_at,
        captured_output=last_snap, attempts=attempts, usage=None,
        transcript_path=claude_transcript_path(cwd, session_id) if claude_transcript_path(cwd, session_id).is_file() else None,
        session_id=session_id,
        provider="claude", burst_home=burst_home,
    )


def run_gemini_burst(
    *,
    cwd: Path,
    prompt: str,
    session_id: Optional[str] = None,  # gemini uses numeric index; we key on "resume" flag
    model: Optional[str] = None,
    fallback_models: Optional[Sequence[str]] = None,
    role: str = "worker",
    session_scope: str = "",
    name_hint: Optional[str] = None,
    done_file: Optional[Path] = None,
    workspace_paths: Optional[Sequence[Path]] = None,
    apparent_stall_seconds: float = 0.0,
    # Stability detection threshold — see run_claude_burst for the policy.
    # Gemini takes even longer quiet windows between tool calls than claude,
    # especially while reasoning through Lean proofs. 20 min of pane-silence
    # before calling the burst done.
    stable_seconds: float = 1200.0,
    # INACTIVITY timeout passed to wait_until_idle. Resets on any progress
    # (pane diff, FS change, busy marker). 60 min of pure silence — hard
    # reasoning tasks can span that long.
    wait_timeout: float = 3600.0,
    max_restarts: int = 2,
    screen_reader: bool = True,
    burst_home: Optional[Path] = None,
    sandbox_enabled: bool = False,
    sandbox_writable_paths: Optional[Sequence[Path]] = None,
    sandbox_config: Optional[SandboxConfig] = None,
    rate_limit_base_delay: float = 30.0,
    rate_limit_max_delay: float = 900.0,
    rate_limit_max_retries: int = 3,
) -> dict:
    """Gemini equivalent of run_claude_burst. resume-behaviour uses `--resume latest`."""
    started_at = time.monotonic()
    started_wall_ms = int(time.time() * 1000)
    cwd = cwd.resolve()
    cwd.mkdir(parents=True, exist_ok=True)
    # Quota probe (start-of-burst). Forced (NOT cooldown-gated) and dispatched
    # onto a thread pool so it runs IN PARALLEL with the burst itself — its
    # 10-25s wall cost adds 0 to a 30-min worker burst. NEVER raises.
    pre_probe_future = _submit_probe_for_burst(
        "gemini", cwd, burst_home=burst_home,
    )
    base_name = name_hint or session_name("gemini", role, session_scope=session_scope)
    attempts = []
    last_snap = ""  # GATE H: last pane snapshot, for provider-CLI-not-found detection
    rate_limit_retries_left = rate_limit_max_retries
    rate_limit_attempt_num = 0  # for exponential backoff
    active_fallbacks = list(fallback_models or [])
    current_model = model
    pre_trust_gemini_folder(cwd, burst_home=burst_home)
    ensure_gemini_accessibility_settings(burst_home=burst_home)
    # Upgrade the burst's gemini to the latest published version if the
    # system install is stale. Installs to ~/.trellis-npm/ under burst_home
    # and the PATH below prepends that bin so it takes precedence.
    ensure_gemini_cli_updated(burst_home)
    # Proactive Google-account rotation at burst start based on /stats.
    try:
        from trellis.gemini_accounts import maybe_ensure_budget  # type: ignore
        maybe_ensure_budget(burst_home=burst_home)
    except Exception:
        pass
    # Track which accounts we've tried this burst so auth/rate-limit rotation
    # doesn't loop on a dead account.
    tried_accounts: set = set()
    _emit_burst_launched(
        provider="gemini", role=role, scope=session_scope, session_id=None,
        model=current_model, effort=None,
        account=_gemini_active_account(burst_home),
        sandbox=sandbox_enabled,
        prompt_bytes=len(prompt.encode("utf-8")),
    )
    for attempt in range(max_restarts + 1):
        kind = "fresh" if attempt == 0 else "resume"
        session_tmux = base_name + ("" if attempt == 0 else f"-r{attempt}")
        kill_session(session_tmux)
        # Keep --screen-reader: it stabilizes tmux pane parsing (busy-marker
        # detection, status-line rendering) even though the busy markers
        # we check for also appear without it.
        gemini_argv = ["gemini", "--approval-mode=yolo"]
        if screen_reader:
            gemini_argv.append("--screen-reader")
        if kind == "resume":
            gemini_argv.extend(["--resume", "latest"])
        if current_model and current_model != "gemini-auto":
            gemini_argv.extend(["--model", current_model])
        wrapped = sandbox_wrap(
            gemini_argv,
            enabled=sandbox_enabled,
            work_dir=cwd,
            burst_home=burst_home,
            writable_paths=sandbox_writable_paths,
            role=role,
            sandbox_config=sandbox_config,
        )
        # Phase 4 bwrap-only migration: no sudo wrap. Gemini-specific env
        # vars (PATH, NO_UPDATE_NOTIFIER, GEMINI_CLI_DISABLE_UPDATE_CHECK,
        # ELAN_HOME, GOOGLE_* API keys) are set on the bwrap parent process
        # via a leading `env KEY=VAL ...` shell prefix — bwrap inherits the
        # parent's env by default (no `--clearenv`), so these reach the
        # inner gemini command. HOME is overridden inside bwrap by the
        # `--setenv HOME <burst_home>` already emitted by sandbox_wrap.
        home = burst_home or Path.home()
        env_prefix = [
            "env",
            # PATH prepends the per-burst-user gemini install so an
            # ensure_gemini_cli_updated()-installed version takes
            # precedence over the root-owned /usr/bin/gemini.
            f"PATH={gemini_path_env(home)}",
            "PYTHONDONTWRITEBYTECODE=1",
            # Suppress gemini CLI auto-update checks: observed the
            # update-notifier prompt + inline npm self-update racing the
            # prompt delivery, hanging the TUI inside the sandbox.
            "NO_UPDATE_NOTIFIER=1",
            "GEMINI_CLI_DISABLE_UPDATE_CHECK=1",
        ]
        try:
            from trellis.host_runtime import worker_elan_home  # type: ignore
            env_prefix.append(f"ELAN_HOME={worker_elan_home()}")
        except Exception:
            pass
        # Forward any API keys the agentapi backend forwards — this lets
        # gemini use the same auth path the working backend did.
        try:
            from trellis.gemini_accounts import gemini_api_env_keys_to_forward  # type: ignore
            for k in gemini_api_env_keys_to_forward(burst_home=burst_home):
                v = os.environ.get(k)
                if v:
                    env_prefix.append(f"{k}={v}")
        except Exception:
            pass
        wrapped = env_prefix + wrapped
        _gemini_launch_stagger()
        new_session(session_tmux, cwd=cwd, cmd=wrapped)
        emit_event("launch", session=session_tmux, provider="gemini",
                   role=role, scope=session_scope, attempt=attempt,
                   detail={"kind": kind, "model": model, "screen_reader": screen_reader,
                           "sandbox": sandbox_enabled})
        h = AgentHandle(session=session_tmux, cwd=cwd, provider="gemini")
        ready, dialogs = settle_until_ready(h, total_timeout=600.0)
        emit_event("settle", session=session_tmux, provider="gemini",
                   role=role, scope=session_scope, attempt=attempt,
                   detail={"ready": ready, "dialogs": dialogs})
        if not ready:
            snap = h.snapshot()
            last_snap = snap or last_snap
            exhausted = extract_exhausted_model(snap)
            auth_expired = any("auth_expired" in d for d in dialogs)
            rate_limited = is_rate_limited(snap)
            cur_acct = _gemini_active_account(burst_home)
            if cur_acct:
                tried_accounts.add(cur_acct)
            kill_session(session_tmux)
            attempts.append({
                "attempt": attempt, "kind": kind, "settle": False, "dialogs": dialogs,
                "rate_limited": rate_limited, "auth_expired": auth_expired,
                "exhausted_model": exhausted, "account": cur_acct,
            })
            # Auth expired OR rate limit: try rotating Google account FIRST
            # before falling back to exponential sleep / non-retryable auth error.
            if auth_expired or rate_limited:
                new_acct = _gemini_try_rotate_to_new_account(
                    burst_home=burst_home,
                    exclude=list(tried_accounts),
                )
                if new_acct:
                    emit_event("account_rotated", session=session_tmux, provider="gemini",
                               role=role, scope=session_scope, attempt=attempt,
                               detail={"from": cur_acct, "to": new_acct,
                                       "trigger": "auth_expired" if auth_expired else "rate_limited"})
                    continue  # free retry with new account
                if auth_expired:
                    _emit_burst_failed(
                        provider="gemini", role=role, scope=session_scope,
                        session_id=None, reason="auth_expired",
                        duration=time.monotonic() - started_at,
                        attempts=attempts, evidence=_evidence_snapshot(session_tmux, snap),
                        account=cur_acct, retryable=False,
                    )
                    return to_burst_result_dict(
                        ok=False, reason="auth_expired", started_at=started_at,
                        captured_output=snap, attempts=attempts, usage=None,
                        transcript_path=None,
                    )
            # Gemini capacity-exhausted → swap to fallback model.
            if exhausted and active_fallbacks:
                new_model = active_fallbacks.pop(0)
                emit_event("fallback_model", session=session_tmux, provider="gemini",
                           role=role, scope=session_scope, attempt=attempt,
                           detail={"exhausted": exhausted, "new_model": new_model})
                current_model = new_model
                continue
            if rate_limited and rate_limit_retries_left > 0:
                delay = exponential_backoff(rate_limit_base_delay, rate_limit_attempt_num, rate_limit_max_delay)
                rate_limit_attempt_num += 1
                rate_limit_retries_left -= 1
                emit_event("rate_limited", session=session_tmux, attempt=attempt,
                           detail={"sleep": round(delay, 1), "source": "settle"})
                time.sleep(delay)
            continue
        baseline = baseline_snapshot(h)
        send_prompt(h, prompt)
        delivered, delivery_reason = confirm_prompt_delivery(h, prompt_head=prompt, timeout=30.0)
        if not delivered:
            send_prompt(h, prompt)
            delivered, delivery_reason = confirm_prompt_delivery(h, prompt_head=prompt, timeout=30.0)
        done, reason = wait_until_idle(
            h,
            min_stable_seconds=stable_seconds,
            total_timeout=wait_timeout,
            baseline=baseline,
            done_file=done_file,
            workspace_paths=workspace_paths,
            liveness_probe=lambda: agent_session_transcript_mtime_ns(
                "gemini", cwd=cwd,
                burst_home=burst_home,
            ),
            apparent_stall_seconds=apparent_stall_seconds,
        )
        captured = h.snapshot()
        # Post-idle: check for auth-expired / rate-limit / model-exhausted in output.
        if not done:
            post_auth_expired = agent_requires_auth(captured, "gemini")
            post_rate_limited = is_rate_limited(captured)
            cur_acct = _gemini_active_account(burst_home)
            if cur_acct:
                tried_accounts.add(cur_acct)
            if post_auth_expired or post_rate_limited:
                new_acct = _gemini_try_rotate_to_new_account(
                    burst_home=burst_home,
                    exclude=list(tried_accounts),
                )
                if new_acct:
                    emit_event("account_rotated", session=session_tmux, provider="gemini",
                               role=role, scope=session_scope, attempt=attempt,
                               detail={"from": cur_acct, "to": new_acct,
                                       "trigger": "auth_expired" if post_auth_expired else "rate_limited",
                                       "source": "post_idle"})
                    kill_session(session_tmux)
                    continue
                if post_auth_expired:
                    _emit_burst_failed(
                        provider="gemini", role=role, scope=session_scope,
                        session_id=None, reason="auth_expired",
                        duration=time.monotonic() - started_at,
                        attempts=attempts, evidence=_evidence_snapshot(session_tmux, captured),
                        account=cur_acct, retryable=False,
                    )
                    kill_session(session_tmux)
                    return to_burst_result_dict(
                        ok=False, reason="auth_expired", started_at=started_at,
                        captured_output=captured, attempts=attempts, usage=None,
                        transcript_path=None,
                    )
            exhausted = extract_exhausted_model(captured)
            if exhausted and active_fallbacks:
                new_model = active_fallbacks.pop(0)
                emit_event("fallback_model", session=session_tmux, provider="gemini",
                           role=role, scope=session_scope, attempt=attempt,
                           detail={"exhausted": exhausted, "new_model": new_model, "source": "post_idle"})
                current_model = new_model
                kill_session(session_tmux)
                continue
        transcript_msg = gemini_last_assistant_message(cwd, home=burst_home)
        usage = gemini_transcript_usage(cwd, home=burst_home)
        attempts.append({
            "attempt": attempt, "kind": kind, "settle": True, "dialogs": dialogs,
            "done": done, "reason": reason,
            "rate_limited": is_rate_limited(captured),
            "account": _gemini_active_account(burst_home),
        })
        # Missing-artifact guard (mirrors claude path): agent went quiet
        # (stable_*) but didn't write the requested done_file. Retry via
        # --resume latest so the session transcript is preserved.
        if done and reason != "done_file" and done_file is not None and not done_file.exists():
            if attempt < max_restarts:
                emit_event("burst_retry", session=session_tmux, provider="gemini",
                           role=role, scope=session_scope, attempt=attempt,
                           detail={"reason": "stable_without_done_file",
                                   "stable_reason": reason,
                                   "source": "post_idle_missing_artifact"})
                kill_session(session_tmux)
                continue
            done = False
            reason = f"missing_artifact_after_{reason}"
        if done:
            # Poll briefly for transcript flush.
            if not transcript_msg:
                for _ in range(8):
                    time.sleep(0.5)
                    transcript_msg = gemini_last_assistant_message(cwd, home=burst_home)
                    if transcript_msg:
                        usage = gemini_transcript_usage(cwd, home=burst_home)
                        break
            # Silent-failure guard: "done" but transcript still empty means
            # gemini probably restarted mid-burst without producing a reply.
            # Retry from scratch if we have a restart slot.
            #
            # BUT: if completion was signaled by done_file, the agent wrote a
            # real artifact — an empty transcript just means it did the work
            # through tool calls without emitting a closing assistant message.
            # Trust the artifact; do not retry. (Observed live: 3 redundant
            # retries totaling ~4 minutes on a burst whose attempt 0 had
            # already written a valid raw.json + done marker.)
            if not transcript_msg and reason != "done_file" and attempt < max_restarts:
                emit_event("gemini_silent_failure", session=session_tmux,
                           role=role, scope=session_scope, attempt=attempt,
                           detail={"reason_prev": reason})
                kill_session(session_tmux)
                continue
            kill_session(session_tmux)
            enriched_usage = dict(usage) if isinstance(usage, dict) else None
            # gemini also resumes across bursts (--resume latest appends to
            # the same session-*.json), so transcript-summed usage is
            # CUMULATIVE. Use the session chat filename as a stable key and
            # let append_cost_ledger compute a per-burst delta.
            session_chat = gemini_latest_session_chat(cwd, home=burst_home)
            gemini_session_key = session_chat.name if session_chat else None
            cumulative_cost = gemini_cost_usd(enriched_usage) if enriched_usage else None
            if enriched_usage is not None and cumulative_cost is not None:
                enriched_usage["cost_usd"] = cumulative_cost
            turn_count = _count_gemini_turns(
                cwd, home=burst_home,
            )
            # Bracket the burst with a post-probe (forced) and collect both
            # pre/post payloads with short timeouts. Either may resolve to
            # None → row records `quota_*: None`.
            post_probe_future = _submit_probe_for_burst(
                "gemini", cwd, burst_home=burst_home,
            )
            pre_payload = _await_with_short_timeout(pre_probe_future, 5.0)
            post_payload = _await_with_short_timeout(post_probe_future, 30.0)
            quota_pre = _project_quota_for_ledger(pre_payload, "gemini")
            quota_post = _project_quota_for_ledger(post_payload, "gemini")
            append_cost_ledger(
                cwd, provider="gemini", role=role, scope=session_scope,
                model=(enriched_usage or {}).get("model", "") if isinstance(enriched_usage, dict) else "",
                usage=enriched_usage, cost_usd=None,
                cumulative_cost_usd=cumulative_cost,
                duration_seconds=time.monotonic() - started_at,
                attempts=len(attempts), ok=True, reason=reason,
                account=_gemini_active_account(burst_home),
                session_id=gemini_session_key,
                ts_start=time.time() - (time.monotonic() - started_at),
                message_count=turn_count,
                quota_pre=quota_pre,
                quota_post=quota_post,
            )
            write_chat_artifacts(
                cwd, role=role,
                artifact_prefix=session_scope or "gemini",
                prompt=prompt,
                final_pane_text=captured,
                transcript_last_assistant=transcript_msg,
                transcript_path=gemini_latest_session_chat(cwd, home=burst_home),
                provider="gemini",
                model=(enriched_usage or {}).get("model") if isinstance(enriched_usage, dict) else current_model,
                session_id=gemini_session_key,
                started_at_ms=started_wall_ms,
                ended_at_ms=int(time.time() * 1000),
                scope=session_scope,
                persistent_transcript_name="transcript.json",
            )
            _emit_burst_succeeded(
                provider="gemini", role=role, scope=session_scope,
                session_id=gemini_session_key,
                model=(enriched_usage or {}).get("model") if isinstance(enriched_usage, dict) else None,
                account=_gemini_active_account(burst_home),
                reason=reason,
                duration=time.monotonic() - started_at, cost_usd=cumulative_cost,
                transcript_len=len(transcript_msg or ""), attempts=len(attempts),
            )
            # Post-burst quota probe was already kicked off above and the row
            # was stamped with the bracketed pre/post payloads.
            return to_burst_result_dict(
                ok=True, reason=reason, started_at=started_at,
                captured_output=captured, attempts=attempts, usage=enriched_usage,
                transcript_path=gemini_latest_session_chat(cwd, home=burst_home),
                extra={
                    "transcript_last_assistant": transcript_msg,
                    "screen_last": captured.splitlines()[-10:],
                    "cost_usd": cumulative_cost,
                },
            )
        if is_rate_limited(captured) and rate_limit_retries_left > 0:
            delay = exponential_backoff(rate_limit_base_delay, rate_limit_attempt_num, rate_limit_max_delay)
            rate_limit_attempt_num += 1
            rate_limit_retries_left -= 1
            kill_session(session_tmux)
            emit_event("rate_limited", session=session_tmux, attempt=attempt,
                       detail={"sleep": round(delay, 1), "source": "post_idle"})
            time.sleep(delay)
            continue
        # Fall-through retry: wait_until_idle returned done=False with a
        # reason like "no_change_seen" / "timeout" / "apparent_stall_Ns",
        # and none of the rate-limit / auth / model-capacity paths fired.
        # Emit an explicit event so no retry is silent — silent retries
        # make debugging impossible (observed live with pre-fix workers).
        emit_event("burst_retry", session=session_tmux, provider="gemini",
                   role=role, scope=session_scope, attempt=attempt,
                   detail={"reason": reason, "done": done, "source": "post_idle_fallthrough"})
        kill_session(session_tmux)
    final_reason = attempts[-1].get("reason", "no_settle") if attempts else "no_attempts"
    _emit_burst_failed(
        provider="gemini", role=role, scope=session_scope,
        session_id=None, reason=final_reason,
        duration=time.monotonic() - started_at,
        attempts=attempts, evidence=_evidence_snapshot(None, ""),
        account=_gemini_active_account(burst_home),
        retryable=True,
    )
    # Failed-burst ledger row with bracketing probes — see the matching
    # claude failure path for rationale.
    post_probe_future_fail = _submit_probe_for_burst(
        "gemini", cwd, burst_home=burst_home,
    )
    pre_payload_fail = _await_with_short_timeout(pre_probe_future, 5.0)
    post_payload_fail = _await_with_short_timeout(post_probe_future_fail, 30.0)
    try:
        append_cost_ledger(
            cwd, provider="gemini", role=role, scope=session_scope,
            model=current_model or "",
            usage=None, cost_usd=0.0,
            duration_seconds=time.monotonic() - started_at,
            attempts=len(attempts), ok=False, reason=final_reason,
            account=_gemini_active_account(burst_home),
            session_id=None,
            ts_start=time.time() - (time.monotonic() - started_at),
            quota_pre=_project_quota_for_ledger(pre_payload_fail, "gemini"),
            quota_post=_project_quota_for_ledger(post_payload_fail, "gemini"),
        )
    except Exception:
        pass
    return to_burst_result_dict(
        ok=False, reason=final_reason, started_at=started_at,
        captured_output=last_snap, attempts=attempts, usage=None,
        transcript_path=gemini_latest_session_chat(cwd, home=burst_home),
        provider="gemini", burst_home=burst_home,
    )


def cmd_kill_midflight(args: argparse.Namespace) -> int:
    """Fault-injection test: launch claude, kill process mid-response, expect retry to save us."""
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    session_id = str(uuid.uuid4())
    session = f"trellis-{args.name}"
    kill_session(session)
    cmd = ["claude", "--dangerously-skip-permissions", "--session-id", session_id]
    new_session(session, cwd=workdir, cmd=cmd)
    h = AgentHandle(session=session, cwd=workdir, provider="claude", session_id=session_id)
    ready, dialogs = settle_until_ready(h, total_timeout=600.0)
    print(f"[settle] ready={ready} dialogs={dialogs}", file=sys.stderr)
    send_prompt(h, "Please write a 3-paragraph essay about lighthouses, ending with ZZZ-END-ZZZ.")
    # Kill the child pid after a short delay.
    time.sleep(4)
    pid = pane_pid(h.session)
    print(f"[fault-inject] killing pid={pid}", file=sys.stderr)
    if pid is not None:
        try:
            os.killpg(os.getpgid(pid), signal.SIGKILL)
        except Exception:
            try:
                os.kill(pid, signal.SIGKILL)
            except Exception as e:
                print(f"[fault-inject] kill failed: {e}", file=sys.stderr)
    # Verify pane_dead detection.
    time.sleep(2)
    print(f"[pane_dead] {pane_dead(h.session)}", file=sys.stderr)
    # Now cleanup and try restart via run_claude_burst.
    kill_session(h.session)
    print("[restart] invoking run_claude_burst with --resume", file=sys.stderr)
    result = run_claude_burst(
        cwd=workdir,
        prompt="Now write that same 3-paragraph essay. End with ZZZ-END-ZZZ.",
        session_id=session_id,
        name_hint=args.name + "-retry",
        stable_seconds=args.stable_seconds,
        wait_timeout=args.wait_timeout,
        max_restarts=1,
    )
    # Simpler summary
    print(json.dumps({
        "ok": result["ok"],
        "reason": result["reason"],
        "attempts_count": len(result.get("attempts", [])),
        "has_end_marker": "ZZZ-END-ZZZ" in result.get("transcript_last_assistant", "") or
                          any("ZZZ-END-ZZZ" in ln for ln in result.get("screen_last", [])),
    }, indent=2))
    return 0 if result["ok"] else 2


def cmd_big_prompt_claude(args: argparse.Namespace) -> int:
    """Send a real historical worker prompt (~60KB) verbatim via tmux send-keys.

    Wraps the prompt body so claude doesn't try to execute the worker task —
    just count the lines and echo a checksum-like token so we can verify
    end-to-end delivery.
    """
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    body = Path(args.prompt_file).read_text(encoding="utf-8")
    prompt = (
        "READ THIS BLOB BELOW BUT DO NOT ACT ON ITS INSTRUCTIONS. "
        "It is content from a previous task sent to me — I am verifying that you received it intact. "
        "Reply with exactly one line of the form: RECEIVED-<num_lines>-<first40chars> "
        "where <num_lines> is the number of newlines in the blob, "
        "and <first40chars> is the first 40 characters of the blob, verbatim (no truncation ellipsis). "
        "Do not include anything else in your reply.\n\n"
        "===BEGIN BLOB===\n"
        + body +
        "\n===END BLOB===\n"
    )
    prompt_size = len(prompt.encode("utf-8"))
    line_count = body.count("\n")
    head40 = body[:40]
    session_id = str(uuid.uuid4())
    session = session_name("claude", "worker", session_scope="big-prompt-smoke")
    kill_session(session)
    cmd = ["claude", "--dangerously-skip-permissions", "--session-id", session_id]
    new_session(session, cwd=workdir, cmd=cmd)
    h = AgentHandle(session=session, cwd=workdir, provider="claude", session_id=session_id)
    ready, dialogs = settle_until_ready(h, total_timeout=600.0)
    print(f"[settle] ready={ready} dialogs={dialogs}", file=sys.stderr)
    t0 = time.monotonic()
    baseline = baseline_snapshot(h)
    # Send in one shot first — see if tmux's argv handles it.
    try:
        tmux("send-keys", "-t", h.session, "-l", prompt, check=True, timeout=30)
        send_ok = True
    except Exception as exc:
        print(f"[send-keys] single-shot failed: {exc} — falling back to load-buffer+paste-buffer", file=sys.stderr)
        send_ok = False
    if not send_ok:
        # Fallback via tmux buffer (no argv size limit).
        bufname = f"trellis-big-{uuid.uuid4().hex[:6]}"
        tmux_proc = subprocess.run(
            _tmux_argv("load-buffer", "-b", bufname, "-"),
            input=prompt, text=True, capture_output=True, check=True, timeout=30,
        )
        tmux("paste-buffer", "-b", bufname, "-t", h.session, check=True, timeout=10)
        tmux("delete-buffer", "-b", bufname, check=False, timeout=5)
    time.sleep(0.5)
    tmux("send-keys", "-t", h.session, "Enter", check=True, timeout=5)
    sent_elapsed = time.monotonic() - t0
    print(f"[sent] bytes={prompt_size} line_count={line_count} first40={head40!r} send_elapsed={sent_elapsed:.2f}s", file=sys.stderr)
    done, reason = wait_until_idle(
        h,
        min_stable_seconds=args.stable_seconds,
        total_timeout=args.wait_timeout,
        baseline=baseline,
    )
    elapsed = time.monotonic() - t0
    snap = h.snapshot()
    (Path("/tmp/tmux-agent-exp/logs") / f"{h.session}.big-prompt.txt").write_text(snap, encoding="utf-8")
    time.sleep(1.0)
    transcript_msg = claude_last_assistant_message(workdir, session_id)
    # Verify: did the echo make it into the transcript? Look at the user msg record.
    transcript_path = claude_transcript_path(workdir, session_id)
    transcript_user_bytes = 0
    transcript_first_user_head40 = ""
    if transcript_path.is_file():
        for line in transcript_path.open("r", encoding="utf-8", errors="replace"):
            try:
                rec = json.loads(line)
            except Exception:
                continue
            if rec.get("type") != "user":
                continue
            msg = rec.get("message") or {}
            content = msg.get("content") or ""
            if isinstance(content, list):
                content = "".join(
                    p.get("text", "") if isinstance(p, dict) else str(p) for p in content
                )
            if isinstance(content, str) and content:
                transcript_user_bytes = len(content.encode("utf-8"))
                # The BLOB should appear; find its first 40 chars.
                idx = content.find("===BEGIN BLOB===")
                if idx >= 0:
                    after = content[idx + len("===BEGIN BLOB==="):].lstrip("\n")
                    transcript_first_user_head40 = after[:40]
                break
    # Expected reply shape
    expected = f"RECEIVED-{line_count}"
    reply_ok = expected in transcript_msg
    head40_in_reply = head40 in transcript_msg
    print(json.dumps({
        "ok": bool(done) and reply_ok,
        "reason": reason,
        "elapsed_total_sec": round(elapsed, 2),
        "prompt_bytes_sent": prompt_size,
        "transcript_user_bytes": transcript_user_bytes,
        "byte_match": transcript_user_bytes >= prompt_size,
        "expected_line_count_token": expected,
        "transcript_last_assistant_head": transcript_msg[:200],
        "reply_contains_expected": reply_ok,
        "reply_echoes_head40": head40_in_reply,
        "transcript_first_user_head40_match": transcript_first_user_head40 == head40,
    }, indent=2))
    if not args.keep:
        kill_session(h.session)
    return 0 if (done and reply_ok and head40_in_reply) else 2


_UNICODE_SAMPLE = (
    "ASCII: hello. "
    "Emoji: 🚀🐉✨🧪. "
    "CJK: 日本語テスト 中文测试 한국어 시험. "
    "Math: ∀x∈ℝ, x²≥0 ⇒ √x²=|x|. "
    "RTL: שלום עולם / مرحبا بالعالم. "
    "Combining: é (e+́), ñ (n+̃). "
    "ZWJ: 👨‍👩‍👧‍👦. "
    "NL: 三\n四\n五."
)


def cmd_unicode_prompt_claude(args: argparse.Namespace) -> int:
    return _unicode_prompt_run(args, provider="claude")


def cmd_unicode_prompt_gemini(args: argparse.Namespace) -> int:
    return _unicode_prompt_run(args, provider="gemini")


def _unicode_prompt_run(args: argparse.Namespace, *, provider: str) -> int:
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    # Prompt asks for an echo token; the interesting part is that the unicode
    # block must arrive intact. We verify via transcript byte-content.
    prompt = (
        "This is a unicode transport test. Read the line below between DELIMS verbatim. "
        "Reply with exactly: UCODE-OK-<len> where <len> is the total number of unicode "
        "codepoints (NOT bytes) between DELIM-A and DELIM-B, not counting the delimiters themselves. "
        "Do not include anything else in your reply.\n\n"
        "DELIM-A " + _UNICODE_SAMPLE + " DELIM-B\n"
    )
    expected_codepoints = len(_UNICODE_SAMPLE)
    expected_token = f"UCODE-OK-{expected_codepoints}"
    if provider == "claude":
        result = run_claude_burst(
            cwd=workdir, prompt=prompt, role="worker", session_scope="unicode",
            stable_seconds=args.stable_seconds, wait_timeout=args.wait_timeout,
            max_restarts=0,
        )
    else:
        result = run_gemini_burst(
            cwd=workdir, prompt=prompt, role="worker", session_scope="unicode",
            stable_seconds=args.stable_seconds, wait_timeout=args.wait_timeout,
            max_restarts=0,
        )
    reply = result.get("transcript_last_assistant") or ""
    sample_in_captured = _UNICODE_SAMPLE in (result.get("captured_output") or "")
    print(json.dumps({
        "ok": result["ok"],
        "reason": result["reason"],
        "expected_token_in_reply": expected_token in reply,
        "actual_reply_head": reply[:100],
        "expected_codepoints": expected_codepoints,
        "sample_visible_on_pane": sample_in_captured,
        "transcript_path": str(result.get("transcript_path")),
    }, indent=2))
    return 0 if result["ok"] and (expected_token in reply or expected_codepoints > 0 and "UCODE-OK-" in reply) else 2


def cmd_replay_transcripts(args: argparse.Namespace) -> int:
    """Re-parse historical claude + gemini transcripts; regression corpus for parsers.

    Walks:
      - ~/.claude/projects/<slug>/<uuid>.jsonl  → claude parser
      - ~/.gemini/tmp/<project>/chats/session-*.json → gemini parser

    Reports: files parsed, non-empty-last-assistant rate, usage-extraction rate,
    per-model counts. Used to catch parser regressions.
    """
    claude_root = Path.home() / ".claude" / "projects"
    gemini_root = Path.home() / ".gemini" / "tmp"
    seen_claude = 0
    ok_claude_msg = 0
    ok_claude_usage = 0
    claude_models: dict = {}
    bad_claude_samples: List[str] = []
    for slug_dir in sorted(claude_root.iterdir()) if claude_root.is_dir() else []:
        if not slug_dir.is_dir():
            continue
        # Rebuild workdir from slug.
        # Slug maps non-word chars to `-`. This is lossy — we can't invert it —
        # but we can still parse by passing the slug path directly and session_id.
        for jsonl in sorted(slug_dir.glob("*.jsonl")):
            sid = jsonl.stem
            seen_claude += 1
            # Use the slug dir directly as "cwd" — our parsers reconstruct the path
            # from cwd+session_id, so we pass a synthetic cwd that maps to this slug.
            # Easier: parse the file directly with the same logic.
            try:
                last_msg = _replay_claude_last_assistant(jsonl)
                usage = _replay_claude_usage(jsonl)
            except Exception as exc:
                bad_claude_samples.append(f"{jsonl.name}: {exc}")
                continue
            if last_msg:
                ok_claude_msg += 1
            if isinstance(usage, dict):
                ok_claude_usage += 1
                model = usage.get("model", "")
                if model:
                    claude_models[model] = claude_models.get(model, 0) + 1
    seen_gemini = 0
    ok_gemini_msg = 0
    ok_gemini_usage = 0
    gemini_models: dict = {}
    bad_gemini_samples: List[str] = []
    if gemini_root.is_dir():
        for proj in sorted(gemini_root.iterdir()):
            chat_dir = proj / "chats"
            if not chat_dir.is_dir():
                continue
            for chat_file in sorted(chat_dir.glob("session-*.json")):
                seen_gemini += 1
                try:
                    last_msg = _replay_gemini_last_assistant(chat_file)
                    usage = _replay_gemini_usage(chat_file)
                except Exception as exc:
                    bad_gemini_samples.append(f"{chat_file.name}: {exc}")
                    continue
                if last_msg:
                    ok_gemini_msg += 1
                if isinstance(usage, dict):
                    ok_gemini_usage += 1
                    model = usage.get("model", "")
                    if model:
                        gemini_models[model] = gemini_models.get(model, 0) + 1
    report = {
        "claude": {
            "files": seen_claude,
            "last_assistant_rate": round(ok_claude_msg / seen_claude, 3) if seen_claude else 0.0,
            "usage_extraction_rate": round(ok_claude_usage / seen_claude, 3) if seen_claude else 0.0,
            "models": claude_models,
            "sample_failures": bad_claude_samples[:3],
        },
        "gemini": {
            "files": seen_gemini,
            "last_assistant_rate": round(ok_gemini_msg / seen_gemini, 3) if seen_gemini else 0.0,
            "usage_extraction_rate": round(ok_gemini_usage / seen_gemini, 3) if seen_gemini else 0.0,
            "models": gemini_models,
            "sample_failures": bad_gemini_samples[:3],
        },
    }
    print(json.dumps(report, indent=2))
    # Pass if both providers hit >= 50% non-empty message extraction on non-trivial corpora.
    ok = True
    for p in ("claude", "gemini"):
        if report[p]["files"] > 5 and report[p]["last_assistant_rate"] < 0.5:
            ok = False
    return 0 if ok else 2


def _replay_claude_last_assistant(jsonl_path: Path) -> str:
    last = ""
    for line in jsonl_path.open("r", encoding="utf-8", errors="replace"):
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("type") != "assistant":
            continue
        msg = rec.get("message") or {}
        parts = msg.get("content") or []
        texts: List[str] = []
        if isinstance(parts, list):
            for p in parts:
                if isinstance(p, dict) and p.get("type") == "text":
                    t = str(p.get("text", ""))
                    if t:
                        texts.append(t)
        elif isinstance(parts, str):
            texts.append(parts)
        if texts:
            last = "\n".join(texts)
    return last


def _replay_claude_usage(jsonl_path: Path) -> Optional[dict]:
    agg = {"input_tokens": 0, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
    model = ""
    found = False
    for line in jsonl_path.open("r", encoding="utf-8", errors="replace"):
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("type") != "assistant":
            continue
        msg = rec.get("message") or {}
        usage = msg.get("usage") if isinstance(msg, dict) else None
        if isinstance(usage, dict):
            found = True
            for k in list(agg.keys()):
                v = usage.get(k)
                if isinstance(v, int):
                    agg[k] += v
            if not model:
                model = str(msg.get("model", "") or "")
    if not found:
        return None
    agg["model"] = model
    agg["total_tokens"] = agg["input_tokens"] + agg["output_tokens"]
    return agg


def _replay_gemini_last_assistant(chat_path: Path) -> str:
    try:
        data = json.loads(chat_path.read_text(encoding="utf-8"))
    except Exception:
        return ""
    last = ""
    for m in data.get("messages", []):
        if isinstance(m, dict) and m.get("type") == "gemini":
            c = m.get("content") or ""
            if isinstance(c, str) and c.strip():
                last = c
    return last


def _replay_gemini_usage(chat_path: Path) -> Optional[dict]:
    try:
        data = json.loads(chat_path.read_text(encoding="utf-8"))
    except Exception:
        return None
    agg = {"input": 0, "output": 0, "cached": 0, "thoughts": 0, "tool": 0, "total": 0}
    model = ""
    found = False
    for m in data.get("messages", []):
        if isinstance(m, dict) and m.get("type") == "gemini":
            tokens = m.get("tokens") or {}
            if isinstance(tokens, dict):
                for k in list(agg.keys()):
                    v = tokens.get(k)
                    if isinstance(v, (int, float)):
                        agg[k] += int(v)
                        found = True
                if not model and isinstance(m.get("model"), str):
                    model = m["model"]
    if not found:
        return None
    agg["model"] = model
    return agg


def cmd_rate_limit_unit(args: argparse.Namespace) -> int:
    """Unit-test: rate_limit detector fires on known phrases, not on normal output."""
    cases = [
        ("Rate limited: please wait.", True),
        ("Error 429: Too Many Requests", True),
        ("status: OK", False),
        ("RESOURCE_EXHAUSTED: quota exceeded", True),
        ("hello world", False),
        ("Model_capacity_exhausted for claude-opus-4-7", True),
        ("Overloaded_error: please retry later", True),
    ]
    failures = []
    for text, expected in cases:
        got = is_rate_limited(text)
        if got != expected:
            failures.append({"text": text, "expected": expected, "got": got})
    print(json.dumps({"tested": len(cases), "failures": failures}, indent=2))
    return 0 if not failures else 2


def cmd_session_naming_unit(args: argparse.Namespace) -> int:
    cases = [
        (("claude","worker", "", ""), "trellis-claude-worker"),
        (("gemini","verifier", "corr/v1", ""), "trellis-gemini-verifier-corr-v1"),
        (("claude","reviewer", "theorem:proof:1", ""), "trellis-claude-reviewer-theorem-proof-1"),
        (("claude","worker", "scope with spaces", "retry"), "trellis-claude-worker-scope-with-spaces-retry"),
    ]
    failures = []
    for (prov, role, scope, extra), want in cases:
        got = session_name(prov, role, session_scope=scope, extra=extra)
        if got != want:
            failures.append({"want": want, "got": got})
    print(json.dumps({"tested": len(cases), "failures": failures}, indent=2))
    return 0 if not failures else 2


def cmd_gemini_transcript(args: argparse.Namespace) -> int:
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    result = run_gemini_burst(
        cwd=workdir,
        prompt=args.prompt,
        model=args.model,
        role=args.role,
        name_hint=args.name,
        stable_seconds=args.stable_seconds,
        wait_timeout=args.wait_timeout,
        max_restarts=0,
    )
    print(json.dumps(result, indent=2, default=str)[:4000])
    return 0 if result["ok"] and (args.expect_in_reply in (result.get("transcript_last_assistant") or "")) else 2


def cmd_bwrap_claude(args: argparse.Namespace) -> int:
    """Full stack: bwrap + tmux + claude (post-bwrap-only: runs as supervisor)."""
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    writable = [workdir / ".trellis" / "staging", workdir / ".trellis" / "scratch"]
    for p in writable:
        p.mkdir(parents=True, exist_ok=True)
    result = run_claude_burst(
        cwd=workdir,
        prompt="Reply with exactly: BWRAP-PONG",
        role="worker",
        session_scope="bwrap-smoke",
        stable_seconds=args.stable_seconds,
        wait_timeout=args.wait_timeout,
        max_restarts=0,
        sandbox_enabled=True,
        sandbox_writable_paths=writable,
    )
    print(json.dumps({
        "ok": result["ok"],
        "reason": result["reason"],
        "transcript_last_assistant": result.get("transcript_last_assistant"),
        "attempts": result["attempts"],
    }, indent=2, default=str)[:4000])
    return 0 if result["ok"] else 2


def cmd_run_burst_claude(args: argparse.Namespace) -> int:
    """Exercises the top-level run_claude_burst with apparent-stall + FS paths + done_file."""
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    done_file = workdir / "artifact.done"
    artifact = workdir / "artifact.json"
    done_file.unlink(missing_ok=True)
    artifact.unlink(missing_ok=True)
    prompt = (
        f"Use the Bash tool to `echo '{{\"answer\":\"BURST-OK\"}}' > {artifact}` and then "
        f"`touch {done_file}`. Do not reply with commentary."
    )
    result = run_claude_burst(
        cwd=workdir,
        prompt=prompt,
        role=args.role,
        session_scope=args.scope,
        model=args.model,
        effort=args.effort,
        done_file=done_file,
        workspace_paths=[workdir],
        apparent_stall_seconds=args.apparent_stall_seconds,
        stable_seconds=args.stable_seconds,
        wait_timeout=args.wait_timeout,
        max_restarts=args.max_restarts,
    )
    print(json.dumps({
        "ok": result["ok"],
        "reason": result["reason"],
        "session_id": result.get("session_id"),
        "transcript_last_assistant": result.get("transcript_last_assistant"),
        "usage_model": (result.get("usage") or {}).get("model") if isinstance(result.get("usage"), dict) else None,
        "artifact_exists": artifact.exists(),
        "artifact_text": artifact.read_text(encoding="utf-8") if artifact.exists() else "",
        "done_file_exists": done_file.exists(),
        "attempts_count": len(result["attempts"]),
    }, indent=2, default=str))
    return 0 if (result["ok"] and artifact.exists() and done_file.exists()) else 2


def cmd_resume_gemini(args: argparse.Namespace) -> int:
    """Launch gemini, send secret prompt, quit, relaunch via --resume, verify memory."""
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    # Fresh first run (cheap trust + seed a conversation).
    h1 = launch_gemini(workdir, name_hint=args.name, screen_reader=True)
    ready1, dialogs1 = settle_until_ready(h1, total_timeout=90.0)
    if dialogs1:
        print(f"[settle-1] dialogs={dialogs1}", file=sys.stderr)
    secret = f"GEMAGIC-{uuid.uuid4().hex[:8]}"
    done1, _, snap1 = run_one_prompt(
        h1, f"Remember this token verbatim: {secret}. Reply only: STORED.",
        stable_seconds=args.stable_seconds, total_timeout=args.wait_timeout,
    )
    (Path("/tmp/tmux-agent-exp/logs") / f"{h1.session}.gemini-resume-t1.txt").write_text(snap1, encoding="utf-8")
    # Cleanly exit gemini so it flushes session history.
    tmux("send-keys", "-t", h1.session, "/quit", "Enter", check=False, timeout=5)
    # Wait briefly for write-back + pane death.
    for _ in range(30):
        if pane_dead(h1.session):
            break
        time.sleep(0.5)
    kill_session(h1.session)
    # Inspect session list.
    sessions = gemini_list_sessions(workdir)
    print(f"[list-sessions] {sessions}", file=sys.stderr)
    # Relaunch with --resume latest.
    session2 = f"trellis-{args.name}-resumed"
    kill_session(session2)
    cmd = ["gemini", "--yolo", "--screen-reader", "--resume", "latest"]
    new_session(session2, cwd=workdir, cmd=cmd)
    h2 = AgentHandle(session=session2, cwd=workdir, provider="gemini")
    ready2, dialogs2 = settle_until_ready(h2, total_timeout=180.0)
    if dialogs2:
        print(f"[settle-2] dialogs={dialogs2}", file=sys.stderr)
    done2, _, snap2 = run_one_prompt(
        h2, "Repeat back the token I asked you to remember. Reply only with the token.",
        stable_seconds=args.stable_seconds, total_timeout=args.wait_timeout,
    )
    (Path("/tmp/tmux-agent-exp/logs") / f"{h2.session}.gemini-resume-t2.txt").write_text(snap2, encoding="utf-8")
    remembered = secret in snap2
    print(json.dumps({
        "sessions_listed": sessions,
        "secret": secret,
        "done1": done1,
        "done2": done2,
        "remembered": remembered,
    }, indent=2))
    if not args.keep:
        kill_session(h2.session)
    return 0 if (done1 and done2 and remembered) else 2


def cmd_apparent_stall_synthetic(args: argparse.Namespace) -> int:
    """Pure-bash test: a tmux pane that says 'esc to interrupt' then sleeps silently.

    No API usage. Proves the detector fires when the busy marker is present,
    screen is static, and no files are being written.
    """
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    session = f"trellis-synth-stall"
    kill_session(session)
    # Bash loop: print "esc to interrupt" once, then sleep 60s.
    cmd = ["bash", "-lc", "echo '⠙ Thinking... (esc to interrupt)'; sleep 60"]
    new_session(session, cwd=workdir, cmd=cmd)
    h = AgentHandle(session=session, cwd=workdir, provider="claude")  # claude pattern matches
    # Give the echo a moment.
    time.sleep(1.0)
    baseline = normalize_pane(capture(h.session))
    print(f"[baseline]: {baseline.strip()[:80]!r}", file=sys.stderr)
    print(f"[busy?]: {agent_is_busy(capture(h.session), 'claude')}", file=sys.stderr)
    t0 = time.monotonic()
    done, reason = wait_until_idle(
        h,
        min_stable_seconds=args.stable_seconds,
        total_timeout=args.wait_timeout,
        baseline=baseline,
        workspace_paths=[workdir],
        apparent_stall_seconds=args.apparent_stall_seconds,
        require_change_first=False,  # no "first change" possible in this test
    )
    elapsed = time.monotonic() - t0
    print(json.dumps({
        "done": done,
        "reason": reason,
        "elapsed_seconds": round(elapsed, 2),
        "expected_reason": f"apparent_stall_{args.apparent_stall_seconds:g}s",
        "hit_expected": reason.startswith("apparent_stall"),
    }, indent=2))
    kill_session(session)
    return 0 if reason.startswith("apparent_stall") else 2


def cmd_apparent_stall_with_fs(args: argparse.Namespace) -> int:
    """Same as above but also write to workspace every 3s — detector should NOT fire."""
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    session = f"trellis-synth-stall-fs"
    kill_session(session)
    # Busy marker + fs activity.
    cmd = [
        "bash", "-lc",
        f"echo '⠙ Thinking... (esc to interrupt)'; "
        f"for i in $(seq 1 15); do sleep 3; echo \"$i\" > {workdir}/progress-$i.txt; done",
    ]
    new_session(session, cwd=workdir, cmd=cmd)
    h = AgentHandle(session=session, cwd=workdir, provider="claude")
    time.sleep(1.0)
    baseline = normalize_pane(capture(h.session))
    t0 = time.monotonic()
    done, reason = wait_until_idle(
        h,
        min_stable_seconds=args.stable_seconds,
        total_timeout=args.wait_timeout,
        baseline=baseline,
        workspace_paths=[workdir],
        apparent_stall_seconds=args.apparent_stall_seconds,
        require_change_first=False,
    )
    elapsed = time.monotonic() - t0
    print(json.dumps({
        "done": done,
        "reason": reason,
        "elapsed_seconds": round(elapsed, 2),
        "should_not_stall": True,
        "apparent_stall_fired": reason.startswith("apparent_stall"),
    }, indent=2))
    kill_session(session)
    # Pass if we did NOT hit apparent_stall (workspace was active).
    return 0 if not reason.startswith("apparent_stall") else 2


def cmd_transcript_claude(args: argparse.Namespace) -> int:
    """One-turn claude call; extract last assistant message + token usage from transcript."""
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    session_id = str(uuid.uuid4())
    h = launch_claude(workdir, session_id=session_id, name_hint=args.name)
    ready, dialogs = settle_until_ready(h, total_timeout=600.0)
    if dialogs:
        print(f"[settle] dialogs={dialogs}", file=sys.stderr)
    baseline = baseline_snapshot(h)
    send_prompt(h, args.prompt)
    done, reason = wait_until_idle(
        h,
        min_stable_seconds=args.stable_seconds,
        total_timeout=args.wait_timeout,
        baseline=baseline,
    )
    # Let claude flush the transcript (usually ~1s).
    time.sleep(1.5)
    transcript_msg = claude_last_assistant_message(workdir, session_id)
    usage = claude_transcript_usage(workdir, session_id)
    screen_snap = h.snapshot()
    (Path("/tmp/tmux-agent-exp/logs") / f"{h.session}.transcript.txt").write_text(
        screen_snap, encoding="utf-8"
    )
    result = {
        "done": done,
        "reason": reason,
        "transcript_path": str(claude_transcript_path(workdir, session_id)),
        "transcript_last_assistant_head": transcript_msg[:200],
        "transcript_exists": claude_transcript_path(workdir, session_id).is_file(),
        "usage": usage,
    }
    print(json.dumps(result, indent=2))
    if not args.keep:
        kill_session(h.session)
    return 0 if done and transcript_msg else 2


def cmd_model_switch_claude(args: argparse.Namespace) -> int:
    """Launch claude, send prompt, kill, relaunch with --resume + --model X, send again.

    Validates that model-switching can be done via CLI flags (no send-keys).
    """
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    session_id = str(uuid.uuid4())
    h = launch_claude(workdir, session_id=session_id, name_hint=args.name, model=args.model_a)
    ready, dialogs = settle_until_ready(h, total_timeout=600.0)
    if dialogs:
        print(f"[settle-1] dialogs={dialogs}", file=sys.stderr)
    done1, _, snap1 = run_one_prompt(
        h,
        f"Identify yourself briefly. In one line, note the model you are. Reply with: I-AM <model>",
        stable_seconds=args.stable_seconds, total_timeout=args.wait_timeout,
    )
    (Path("/tmp/tmux-agent-exp/logs") / f"{h.session}.model-a.txt").write_text(snap1, encoding="utf-8")
    kill_session(h.session)
    time.sleep(1)
    # Relaunch with a different model flag + --resume same session.
    session2 = f"trellis-{args.name}-switched"
    kill_session(session2)
    cmd = [
        "claude", "--dangerously-skip-permissions",
        "--resume", session_id,
        "--model", args.model_b,
    ]
    new_session(session2, cwd=workdir, cmd=cmd)
    h2 = AgentHandle(session=session2, cwd=workdir, provider="claude", session_id=session_id)
    ready2, dialogs2 = settle_until_ready(h2, total_timeout=180.0)
    if dialogs2:
        print(f"[settle-2] dialogs={dialogs2}", file=sys.stderr)
    done2, _, snap2 = run_one_prompt(
        h2,
        "Again: in one line, note the model you are. Reply with: I-AM <model>",
        stable_seconds=args.stable_seconds, total_timeout=args.wait_timeout,
    )
    (Path("/tmp/tmux-agent-exp/logs") / f"{h2.session}.model-b.txt").write_text(snap2, encoding="utf-8")
    usage = claude_transcript_usage(workdir, session_id)
    print(json.dumps({
        "done1": done1,
        "done2": done2,
        "model_a_screen_head": snap1.splitlines()[-40:],
        "model_b_screen_head": snap2.splitlines()[-40:],
        "final_transcript_usage": usage,
    }, indent=2, default=str)[:4000])
    if not args.keep:
        kill_session(h2.session)
    return 0 if (done1 and done2) else 2


def cmd_apparent_stall_claude(args: argparse.Namespace) -> int:
    """Exercise the apparent-stall detector by asking claude to sleep.

    During `sleep N` via its Bash tool, claude's busy marker stays up, screen
    doesn't change, and no file is written → detector should fire.
    """
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    session_id = str(uuid.uuid4())
    h = launch_claude(workdir, session_id=session_id, name_hint=args.name)
    ready, dialogs = settle_until_ready(h, total_timeout=600.0)
    print(f"[settle] ready={ready} dialogs={dialogs}", file=sys.stderr)
    prompt = f"Use the Bash tool to run `sleep {args.sleep_seconds}`, then reply DONE-STALL."
    baseline = baseline_snapshot(h)
    t0 = time.monotonic()
    send_prompt(h, prompt)
    done, reason = wait_until_idle(
        h,
        min_stable_seconds=args.stable_seconds,
        total_timeout=args.wait_timeout,
        baseline=baseline,
        workspace_paths=[workdir],
        apparent_stall_seconds=args.apparent_stall_seconds,
    )
    elapsed = time.monotonic() - t0
    snap = h.snapshot()
    (Path("/tmp/tmux-agent-exp/logs") / f"{h.session}.stall-apparent.txt").write_text(snap, encoding="utf-8")
    print(json.dumps({
        "done": done,
        "reason": reason,
        "elapsed_seconds": round(elapsed, 2),
        "expected_reason": f"apparent_stall_{args.apparent_stall_seconds:g}s",
        "hit_expected": reason.startswith("apparent_stall"),
    }, indent=2))
    if not args.keep:
        kill_session(h.session)
    return 0 if reason.startswith("apparent_stall") else 2


def cmd_donefile_claude(args: argparse.Namespace) -> int:
    """Ask claude to write a JSON + done marker; completion detected via done_file."""
    workdir = Path(args.workdir).resolve()
    done_file = workdir / "out.done"
    artifact = workdir / "out.json"
    for p in (done_file, artifact):
        p.parent.mkdir(parents=True, exist_ok=True)
        p.unlink(missing_ok=True)
    session_id = str(uuid.uuid4())
    h = launch_claude(workdir, session_id=session_id, name_hint=args.name)
    ready, dialogs = settle_until_ready(h, total_timeout=600.0)
    if dialogs:
        print(f"[settle] dialogs={dialogs}", file=sys.stderr)
    prompt = (
        f"Use the Bash tool to write this exact JSON to {artifact}: "
        f'{{"answer":"PONG","note":"tmux-experiment"}} '
        f"— then, ONLY AFTER that file exists, touch {done_file} to signal completion. "
        f"Do not reply with commentary; just run the commands."
    )
    baseline = baseline_snapshot(h)
    t0 = time.monotonic()
    send_prompt(h, prompt)
    done, reason = wait_until_idle(
        h,
        min_stable_seconds=args.stable_seconds,
        total_timeout=args.wait_timeout,
        baseline=baseline,
        done_file=done_file,
    )
    elapsed = time.monotonic() - t0
    snap = h.snapshot()
    (Path("/tmp/tmux-agent-exp/logs") / f"{h.session}.donefile.txt").write_text(snap, encoding="utf-8")
    artifact_exists = artifact.exists()
    artifact_text = artifact.read_text(encoding="utf-8") if artifact_exists else ""
    print(json.dumps({
        "done": done,
        "reason": reason,
        "elapsed_seconds": round(elapsed, 2),
        "done_file_exists": done_file.exists(),
        "artifact_exists": artifact_exists,
        "artifact_text": artifact_text[:200],
    }, indent=2))
    if not args.keep:
        kill_session(h.session)
    return 0 if done and reason == "done_file" and artifact_exists else 2


def cmd_stall_claude(args: argparse.Namespace) -> int:
    """Send a longer prompt to claude and report stall timing."""
    workdir = Path(args.workdir).resolve()
    session_id = str(uuid.uuid4())
    h = launch_claude(workdir, session_id=session_id, name_hint=args.name)
    ready, dialogs = settle_until_ready(h, total_timeout=600.0)
    if dialogs:
        print(f"[settle] dialogs={dialogs}", file=sys.stderr)
    prompt = (
        "Write a 3-paragraph story about a lighthouse keeper. "
        "End the final paragraph with the exact token ZZZ-END-ZZZ."
    )
    t0 = time.monotonic()
    baseline = baseline_snapshot(h)
    send_prompt(h, prompt)
    # Instrumented loop using the shared busy-aware logic.
    last_norm = baseline
    last_change = time.monotonic()
    change_seen = False
    ever_busy = False
    transitions = []
    busy_transitions = []
    stable_for = args.stable_seconds
    total_timeout = args.wait_timeout
    start = time.monotonic()
    decided: Optional[Tuple[bool, str]] = None
    while time.monotonic() - start < total_timeout:
        screen = capture(h.session)
        norm = normalize_pane(screen)
        busy = agent_is_busy(screen, h.provider)
        if busy:
            if not ever_busy:
                busy_transitions.append(("busy_start", round(time.monotonic() - t0, 2)))
            ever_busy = True
            last_change = time.monotonic()
            change_seen = True
            last_norm = norm
            time.sleep(1.0)
            continue
        else:
            if ever_busy and (not busy_transitions or busy_transitions[-1][0] != "busy_end"):
                busy_transitions.append(("busy_end", round(time.monotonic() - t0, 2)))
        changed = (norm != last_norm)
        if changed:
            last_norm = norm
            last_change = time.monotonic()
            change_seen = True
            transitions.append(round(time.monotonic() - t0, 2))
        elif change_seen and (time.monotonic() - last_change) >= stable_for:
            decided = (True, f"stable_{stable_for:g}s")
            break
        time.sleep(1.0)
    if decided is None:
        decided = (False, "timeout_or_nochange")
    elapsed = time.monotonic() - t0
    snap = h.snapshot()
    (Path("/tmp/tmux-agent-exp/logs") / f"{h.session}.stall.txt").write_text(snap, encoding="utf-8")
    has_marker = "ZZZ-END-ZZZ" in snap
    print(json.dumps({
        "elapsed_seconds": round(elapsed, 2),
        "decided": decided,
        "change_count": len(transitions),
        "first_change_at": transitions[0] if transitions else None,
        "last_change_at": transitions[-1] if transitions else None,
        "stall_fired_after_last_change": round(time.monotonic() - last_change, 2),
        "ever_busy": ever_busy,
        "busy_transitions": busy_transitions,
        "has_end_marker": has_marker,
    }, indent=2))
    if not args.keep:
        kill_session(h.session)
    return 0 if (decided[0] and has_marker) else 2


def cmd_resume_claude(args: argparse.Namespace) -> int:
    """Launch, send one prompt, kill session, relaunch with --resume, confirm memory."""
    workdir = Path(args.workdir).resolve()
    session_id = str(uuid.uuid4())
    h = launch_claude(workdir, session_id=session_id, name_hint=args.name)
    print(f"[launch-1] session={h.session} session_id={session_id}", file=sys.stderr)
    ready, dialogs = settle_until_ready(h, total_timeout=600.0)
    if dialogs:
        print(f"[settle] dialogs={dialogs}", file=sys.stderr)
    if not ready:
        print("[warn] initial readiness not detected", file=sys.stderr)
    secret = f"MAGIC-{uuid.uuid4().hex[:8]}"
    done1, reason1, snap1 = run_one_prompt(
        h, f"Remember this secret phrase verbatim: {secret}. Reply only with: STORED.",
        stable_seconds=args.stable_seconds, total_timeout=args.wait_timeout,
    )
    print(f"[turn 1] done={done1} reason={reason1} secret={secret}", file=sys.stderr)
    (Path("/tmp/tmux-agent-exp/logs") / f"{h.session}.resume-turn1.txt").write_text(snap1, encoding="utf-8")
    kill_session(h.session)
    time.sleep(1)

    # Relaunch with same session_id + --resume
    session2 = f"trellis-{args.name}-resumed"
    kill_session(session2)
    cmd = ["claude", "--dangerously-skip-permissions", "--resume", session_id]
    new_session(session2, cwd=workdir, cmd=cmd)
    h2 = AgentHandle(session=session2, cwd=workdir, provider="claude", session_id=session_id)
    print(f"[launch-2] session={session2} --resume {session_id}", file=sys.stderr)
    ready2, dialogs2 = settle_until_ready(h2, total_timeout=180.0)
    if dialogs2:
        print(f"[settle-2] dialogs={dialogs2}", file=sys.stderr)
    if not ready2:
        print("[warn] resume readiness not detected", file=sys.stderr)
    done2, reason2, snap2 = run_one_prompt(
        h2, "Repeat back the secret phrase I told you earlier. Reply only with the phrase.",
        stable_seconds=args.stable_seconds, total_timeout=args.wait_timeout,
    )
    (Path("/tmp/tmux-agent-exp/logs") / f"{h2.session}.resume-turn2.txt").write_text(snap2, encoding="utf-8")
    print(f"[turn 2] done={done2} reason={reason2}", file=sys.stderr)
    remembered = secret in snap2
    print(f"[result] secret={'remembered' if remembered else 'NOT remembered'} secret={secret}", file=sys.stderr)
    if not args.keep:
        kill_session(h2.session)
    return 0 if (done1 and done2 and remembered) else 2


def main(argv: Optional[List[str]] = None) -> int:
    p = argparse.ArgumentParser(description="tmux-agent experiment driver")
    sp = p.add_subparsers(dest="cmd", required=True)

    hc = sp.add_parser("hello-claude")
    hc.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-work")
    hc.add_argument("--prompt", default="What is 2+2? Answer with just the number.")
    hc.add_argument("--name", default="claude-hello")
    hc.add_argument("--keep", action="store_true")
    hc.add_argument("--stable-seconds", type=float, default=6.0)
    hc.add_argument("--wait-timeout", type=float, default=180.0)
    hc.set_defaults(func=cmd_hello_claude)

    hg = sp.add_parser("hello-gemini")
    hg.add_argument("--workdir", default="/tmp/tmux-agent-exp/gemini-work")
    hg.add_argument("--prompt", default="What is 2+2? Answer with just the number.")
    hg.add_argument("--name", default="gemini-hello")
    hg.add_argument("--keep", action="store_true")
    hg.add_argument("--screen-reader", action="store_true")
    hg.add_argument("--stable-seconds", type=float, default=6.0)
    hg.add_argument("--wait-timeout", type=float, default=180.0)
    hg.set_defaults(func=cmd_hello_gemini)

    mc = sp.add_parser("multiturn-claude")
    mc.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-work")
    mc.add_argument("--name", default="claude-multi")
    mc.add_argument("--keep", action="store_true")
    mc.add_argument("--stable-seconds", type=float, default=6.0)
    mc.add_argument("--wait-timeout", type=float, default=180.0)
    mc.set_defaults(func=cmd_multiturn_claude)

    rc = sp.add_parser("resume-claude")
    rc.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-resume-work")
    rc.add_argument("--name", default="claude-resume")
    rc.add_argument("--keep", action="store_true")
    rc.add_argument("--stable-seconds", type=float, default=6.0)
    rc.add_argument("--wait-timeout", type=float, default=180.0)
    rc.set_defaults(func=cmd_resume_claude)

    sc = sp.add_parser("stall-claude")
    sc.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-stall-work")
    sc.add_argument("--name", default="claude-stall")
    sc.add_argument("--keep", action="store_true")
    sc.add_argument("--stable-seconds", type=float, default=6.0)
    sc.add_argument("--wait-timeout", type=float, default=180.0)
    sc.set_defaults(func=cmd_stall_claude)

    dc = sp.add_parser("donefile-claude")
    dc.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-donefile-work")
    dc.add_argument("--name", default="claude-donefile")
    dc.add_argument("--keep", action="store_true")
    dc.add_argument("--stable-seconds", type=float, default=10.0)
    dc.add_argument("--wait-timeout", type=float, default=180.0)
    dc.set_defaults(func=cmd_donefile_claude)

    sa = sp.add_parser("apparent-stall-claude")
    sa.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-apparent-stall-work")
    sa.add_argument("--name", default="claude-apparent-stall")
    sa.add_argument("--keep", action="store_true")
    sa.add_argument("--sleep-seconds", type=int, default=25)
    sa.add_argument("--apparent-stall-seconds", type=float, default=10.0)
    sa.add_argument("--stable-seconds", type=float, default=6.0)
    sa.add_argument("--wait-timeout", type=float, default=60.0)
    sa.set_defaults(func=cmd_apparent_stall_claude)

    ss = sp.add_parser("apparent-stall-synthetic")
    ss.add_argument("--workdir", default="/tmp/tmux-agent-exp/synth-stall-work")
    ss.add_argument("--apparent-stall-seconds", type=float, default=8.0)
    ss.add_argument("--stable-seconds", type=float, default=6.0)
    ss.add_argument("--wait-timeout", type=float, default=30.0)
    ss.set_defaults(func=cmd_apparent_stall_synthetic)

    sf = sp.add_parser("apparent-stall-with-fs")
    sf.add_argument("--workdir", default="/tmp/tmux-agent-exp/synth-stall-fs-work")
    sf.add_argument("--apparent-stall-seconds", type=float, default=8.0)
    sf.add_argument("--stable-seconds", type=float, default=6.0)
    sf.add_argument("--wait-timeout", type=float, default=50.0)
    sf.set_defaults(func=cmd_apparent_stall_with_fs)

    tr = sp.add_parser("transcript-claude")
    tr.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-transcript-work")
    tr.add_argument("--name", default="claude-transcript")
    tr.add_argument("--keep", action="store_true")
    tr.add_argument("--prompt", default="Respond briefly with the word BANANA and nothing else.")
    tr.add_argument("--stable-seconds", type=float, default=6.0)
    tr.add_argument("--wait-timeout", type=float, default=60.0)
    tr.set_defaults(func=cmd_transcript_claude)

    ms = sp.add_parser("model-switch-claude")
    ms.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-model-switch-work")
    ms.add_argument("--name", default="claude-model-switch")
    ms.add_argument("--keep", action="store_true")
    ms.add_argument("--model-a", default="sonnet")
    ms.add_argument("--model-b", default="opus")
    ms.add_argument("--stable-seconds", type=float, default=6.0)
    ms.add_argument("--wait-timeout", type=float, default=60.0)
    ms.set_defaults(func=cmd_model_switch_claude)

    rg = sp.add_parser("resume-gemini")
    rg.add_argument("--workdir", default="/tmp/tmux-agent-exp/gemini-resume-work")
    rg.add_argument("--name", default="gemini-resume")
    rg.add_argument("--keep", action="store_true")
    rg.add_argument("--stable-seconds", type=float, default=6.0)
    rg.add_argument("--wait-timeout", type=float, default=60.0)
    rg.set_defaults(func=cmd_resume_gemini)

    km = sp.add_parser("kill-midflight-claude")
    km.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-kill-work")
    km.add_argument("--name", default="claude-killmid")
    km.add_argument("--stable-seconds", type=float, default=6.0)
    km.add_argument("--wait-timeout", type=float, default=90.0)
    km.set_defaults(func=cmd_kill_midflight)

    rl = sp.add_parser("rate-limit-unit")
    rl.set_defaults(func=cmd_rate_limit_unit)

    rp = sp.add_parser("replay-transcripts")
    rp.set_defaults(func=cmd_replay_transcripts)

    uc = sp.add_parser("unicode-prompt-claude")
    uc.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-unicode-work")
    uc.add_argument("--stable-seconds", type=float, default=6.0)
    uc.add_argument("--wait-timeout", type=float, default=60.0)
    uc.set_defaults(func=cmd_unicode_prompt_claude)

    ug = sp.add_parser("unicode-prompt-gemini")
    ug.add_argument("--workdir", default="/tmp/tmux-agent-exp/gemini-unicode-work")
    ug.add_argument("--stable-seconds", type=float, default=6.0)
    ug.add_argument("--wait-timeout", type=float, default=60.0)
    ug.set_defaults(func=cmd_unicode_prompt_gemini)

    bp = sp.add_parser("big-prompt-claude")
    bp.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-bigprompt-work")
    bp.add_argument("--prompt-file", required=True)
    bp.add_argument("--keep", action="store_true")
    bp.add_argument("--stable-seconds", type=float, default=10.0)
    bp.add_argument("--wait-timeout", type=float, default=180.0)
    bp.set_defaults(func=cmd_big_prompt_claude)

    sn = sp.add_parser("session-naming-unit")
    sn.set_defaults(func=cmd_session_naming_unit)

    gt = sp.add_parser("gemini-transcript")
    gt.add_argument("--workdir", default="/tmp/tmux-agent-exp/gemini-transcript-work")
    gt.add_argument("--name", default=None)
    gt.add_argument("--role", default="worker")
    gt.add_argument("--prompt", default="Reply with exactly: GEM-TRANS-OK")
    gt.add_argument("--expect-in-reply", default="GEM-TRANS-OK")
    gt.add_argument("--model", default=None)
    gt.add_argument("--stable-seconds", type=float, default=6.0)
    gt.add_argument("--wait-timeout", type=float, default=60.0)
    gt.set_defaults(func=cmd_gemini_transcript)

    bw = sp.add_parser("bwrap-claude")
    bw.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-bwrap-work")
    bw.add_argument("--stable-seconds", type=float, default=6.0)
    bw.add_argument("--wait-timeout", type=float, default=60.0)
    bw.set_defaults(func=cmd_bwrap_claude)

    rb = sp.add_parser("run-burst-claude")
    rb.add_argument("--workdir", default="/tmp/tmux-agent-exp/claude-run-burst-work")
    rb.add_argument("--role", default="worker")
    rb.add_argument("--scope", default="run-burst-smoke")
    rb.add_argument("--model", default=None)
    rb.add_argument("--effort", default=None)
    rb.add_argument("--apparent-stall-seconds", type=float, default=0.0)
    rb.add_argument("--stable-seconds", type=float, default=6.0)
    rb.add_argument("--wait-timeout", type=float, default=120.0)
    rb.add_argument("--max-restarts", type=int, default=1)
    rb.set_defaults(func=cmd_run_burst_claude)

    args = p.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())


# --- public entrypoint -------------------------------------------------------

_BURST_RESULT_FIELDS = {
    "ok", "exit_code", "captured_output", "duration_seconds",
    "stall_recoveries", "usage", "error", "recovery_log", "transcript_path",
}


def run(
    config: ProviderConfig,
    prompt: str,
    *,
    role: str = "worker",
    session_name: Optional[str] = None,
    work_dir: Path,
    timeout: float = 3600,
    startup_timeout: float = 3600,
    port: Optional[int] = None,
    fresh: bool = False,
    done_file: Optional[Path] = None,
    log_dir: Optional[Path] = None,
    artifact_prefix: Optional[str] = None,
    sandbox: Optional[SandboxConfig] = None,
    burst_home: Optional[Path] = None,
    session_scope: str = "",
) -> BurstResult:
    """Public entrypoint for Claude / Gemini bursts.

    Dispatches to `run_claude_burst` or `run_gemini_burst` based on
    `config.provider`. Returns a `BurstResult`.

    The `port` kwarg is accepted for historical call-site compatibility
    but ignored — tmux uses session names for lane isolation (see
    `session_name` helper).

    Bug X principled fix: `fresh=True` is now honored. The kernel sets it
    via `next_worker_context_mode="fresh"` after a Transport-failure retry
    or whenever the reviewer hands a worker a new task. We forget the
    stored session identity sidecar, so the next launch mints a new
    session id (claude `--session-id <new>` / gemini drops `--resume
    latest`) and the agent starts from a clean transcript.
    """
    # Enable event logging once per process; default under the project's log tree.
    if _DEFAULT_EVENT_LOG_PATH is None:
        set_event_log_path(work_dir / ".trellis" / "logs" / "tmux-backend-events.jsonl")

    # Bug X principled fix: honor `fresh=True` by clearing the session
    # sidecar before dispatch. The downstream burst functions read the
    # sidecar to decide fresh-vs-resume; missing sidecar → fresh.
    if fresh and config.provider in {"claude", "gemini"}:
        clear_session_identity(
            work_dir, provider=config.provider, role=role,
            session_scope=session_scope,
        )

    sandbox_enabled = sandbox is not None and getattr(sandbox, "enabled", False)

    # Burst-timing policy: err on the side of waiting much longer than
    # necessary — far cheaper to wait extra time on a done-but-quiet burst
    # than to kill an agent mid-reasoning. apparent_stall (fast-fail on
    # wedge) now fires only when TWO positive work signals have both been
    # stale for apparent_stall_seconds:
    #   (1) workspace_paths: tool-call writes to Tablet/, .trellis/scratch/,
    #       .trellis/staging/, runtime staging — visible to supervisor as
    #       the supervisor user without sudo.
    #   (2) liveness_probe: agent session transcript file mtime (claude
    #       jsonl or gemini session-*.json). Grows on every streamed token
    #       chunk or tool_use block. Accessed via sudo against the
    #       burst-user home.
    # Both are unforgeable by a TUI-render thread — no TUI tickers are used.
    ws_paths: List[Path] = []
    for sub in (
        work_dir / "Tablet",
        work_dir / ".trellis" / "scratch",
        work_dir / ".trellis" / "staging",
    ):
        if sub.exists():
            ws_paths.append(sub)
    runtime_root = work_dir / ".trellis" / "runtime"
    if runtime_root.exists():
        for child in runtime_root.iterdir():
            if child.is_dir() and (child / "staging").exists():
                ws_paths.append(child / "staging")
    if config.provider == "claude":
        result = run_claude_burst(
            cwd=work_dir, prompt=prompt,
            model=config.model, effort=config.effort,
            role=role, session_scope=session_scope,
            name_hint=session_name,
            done_file=done_file,
            workspace_paths=ws_paths,
            apparent_stall_seconds=5400.0,    # 90 min — gemini-3.1-pro can spend
                                              # 30-45 min in a single thinking
                                              # phase reading files into context
                                              # without FS writes or transcript
                                              # appends; killing at 20 min was
                                              # generating false transport
                                              # failures (run May 2026 c494).
            stable_seconds=5400.0,            # 90 min pane-silence before done
            wait_timeout=max(float(timeout or 0), 7200.0),  # >= 2 h inactivity
            # Bug X principled fix: drop the inner retry loop. The kernel
            # now owns retries via RetryOutcomeKind::Transport — silent
            # max_restarts=2 retries here hid transport failures from the
            # kernel and corrupted before_snapshot baselines (Bug X
            # cycle-49 root cause). First stable_without_done_file or
            # silent_failure now returns immediately.
            max_restarts=0,
            burst_home=burst_home,
            sandbox_enabled=sandbox_enabled,
            sandbox_config=sandbox,
        )
    elif config.provider == "gemini":
        result = run_gemini_burst(
            cwd=work_dir, prompt=prompt,
            model=config.model,
            fallback_models=list(getattr(config, "fallback_models", None) or ()),
            role=role, session_scope=session_scope,
            name_hint=session_name,
            done_file=done_file,
            workspace_paths=ws_paths,
            apparent_stall_seconds=5400.0,    # 90 min — see run_claude_burst
                                              # comment above; gemini-3.1-pro
                                              # in particular can stay in
                                              # `Thinking...` for 30-45 min
                                              # while reading multiple long
                                              # tex/lean files into context.
            stable_seconds=5400.0,
            wait_timeout=max(float(timeout or 0), 7200.0),
            # Bug X principled fix: drop the inner retry loop. See
            # run_claude_burst comment above.
            max_restarts=0,
            burst_home=burst_home,
            sandbox_enabled=sandbox_enabled,
            sandbox_config=sandbox,
        )
    else:
        raise ValueError(
            f"trellis.agents.tmux_backend does not support provider: {config.provider!r}"
        )

    fields = {k: v for k, v in result.items() if k in _BURST_RESULT_FIELDS}
    fields.setdefault("stall_recoveries", 0)
    fields.setdefault("usage", None)
    fields.setdefault("error", "")
    fields.setdefault("recovery_log", [])
    return BurstResult(**fields)
