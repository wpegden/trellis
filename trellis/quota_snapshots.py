"""Per-provider quota snapshots.

Captures longest-cadence quota state (weekly for claude/codex; daily for
gemini's per-model caps) on a 30-minute cooldown. The data is
**cosmetic-only**: every probe is wrapped in a broad try/except and any
failure logs `{provider, error}` to the snapshots ledger and proceeds.
A run NEVER breaks because of a quota probe.

Output: `.trellis/logs/quota-snapshots.jsonl`, one row per probe.

Hook points (see `maybe_probe_provider` below):
  - At the start of every agent burst, before dispatch.
  - At the end of every agent burst, after the cost-ledger row is written.
The 30-min cooldown gates both — so in a busy run probes fire at most
once every 30 min per provider; in a slow run they fire near every burst.
A successful probe pair (start + end of one burst) gives a within-burst
delta on the percent-used field, which is the canonical "burn rate" signal.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import tempfile
import threading
import time
import traceback
import uuid
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

from trellis.host_runtime import worker_path_env
from trellis.tmux_socket import tmux_argv

LEDGER_FILENAME = "quota-snapshots.jsonl"
COOLDOWN_SECONDS = 30 * 60  # 30 min — see Q1 in the design doc

# Approximate fraction of a 30-day month covered by a window of given seconds.
_HOURS_PER_MONTH = 30 * 24
_SECONDS_PER_MONTH = _HOURS_PER_MONTH * 3600


def _frac_of_month(window_seconds: float) -> float:
    return float(window_seconds) / float(_SECONDS_PER_MONTH)


# ----- per-provider cooldown gating ----------------------------------------

# Process-local. The supervisor is a single process, so this is sufficient
# for the "30 min since last probe" gate. Across supervisor restarts, the
# gate resets — that's fine: the first probe per restart is genuinely useful.
_LAST_PROBE_AT: Dict[str, float] = {}

# Circuit breaker. Quota probes are fragile (TUI rendering, CLI version
# drift). After N consecutive failures for a provider, suspend probing
# that provider for the rest of the supervisor process. The supervisor
# itself MUST keep running — drift in quota tracking is recoverable;
# breaking the run is not.
_CONSECUTIVE_FAILURES: Dict[str, int] = {}
_SUSPENDED_PROVIDERS: set = set()
PROBE_FAILURE_THRESHOLD = 3


def should_probe(
    provider: str,
    *,
    now: Optional[float] = None,
    repo: Optional[Path] = None,
) -> bool:
    """Return True iff this provider should be probed now: not suspended by
    the circuit breaker AND COOLDOWN_SECONDS has elapsed since the last probe.

    If `repo` is provided and there's no in-process record of a prior probe
    for this provider, consults the on-disk snapshots ledger for the most
    recent ts. Necessary because the supervisor spawns a fresh Python
    subprocess for each burst — without this, _LAST_PROBE_AT would reset
    every subprocess and every burst would fire a probe.
    """
    if provider in _SUSPENDED_PROVIDERS:
        return False
    t = now if now is not None else time.time()
    last = _LAST_PROBE_AT.get(provider)
    if last is None and repo is not None:
        last = _ledger_last_probe_ts(provider, repo)
        if last is not None:
            _LAST_PROBE_AT[provider] = last
    if last is None:
        return True
    return (t - last) >= COOLDOWN_SECONDS


def _ledger_last_probe_ts(provider: str, repo: Path) -> Optional[float]:
    """Scan the snapshots ledger for the most recent ts of a probe (success
    OR failure) for this provider. Failures count too — they consumed a
    probe attempt and we don't want to retry instantly. Best-effort.
    """
    try:
        path = default_ledger_path(repo)
        if not path.is_file():
            return None
        latest: Optional[float] = None
        with path.open("r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    r = json.loads(line)
                except Exception:
                    continue
                if r.get("provider") != provider:
                    continue
                ts = r.get("ts")
                if isinstance(ts, (int, float)) and (latest is None or ts > latest):
                    latest = float(ts)
        return latest
    except Exception:
        return None


def mark_probed(provider: str, *, now: Optional[float] = None) -> None:
    _LAST_PROBE_AT[provider] = now if now is not None else time.time()


def _record_probe_outcome(provider: str, ok: bool) -> None:
    if ok:
        _CONSECUTIVE_FAILURES[provider] = 0
        return
    n = _CONSECUTIVE_FAILURES.get(provider, 0) + 1
    _CONSECUTIVE_FAILURES[provider] = n
    if n >= PROBE_FAILURE_THRESHOLD:
        _SUSPENDED_PROVIDERS.add(provider)


# ----- ledger ---------------------------------------------------------------

def default_ledger_path(repo: Path) -> Path:
    return Path(repo) / ".trellis" / "logs" / LEDGER_FILENAME


def append_quota_snapshot(repo: Path, payload: Dict[str, Any]) -> None:
    """Append one JSON line to the quota snapshots ledger.

    Best-effort: I/O errors are swallowed. Quota tracking must never
    block a run.
    """
    try:
        path = default_ledger_path(repo)
        path.parent.mkdir(parents=True, exist_ok=True)
        rec = {"ts": time.time(), **payload}
        with path.open("a", encoding="utf-8") as f:
            f.write(json.dumps(rec, default=str) + "\n")
    except Exception:
        pass


# ----- claude probe (interactive `/usage`, ~10-15s, $0) --------------------
#
# Earlier iteration shelled out to `claude -p --output-format=stream-json`
# to harvest one `rate_limit_event`. That only ever surfaced the
# *currently-active* rate limit type — for Max accounts that's almost
# always `five_hour`, never the weekly window we actually care about
# tracking. The interactive `/usage` slash command is the only place
# claude exposes the weekly limit, with explicit "all models" + per-model
# breakdowns and reset timestamps.
#
# Cost: $0 (slash command, no model invocation). ~10-15s wall.


def probe_claude(
    *,
    burst_home: Optional[Path] = None,
    timeout_seconds: float = 60.0,
) -> Dict[str, Any]:
    """Spawn claude in a throwaway tmux session, send `/usage`, parse the
    panel for the current 5-hour ("Current session") and weekly windows.

    Returns a normalized payload. On any failure: `{"ok": False, "error": ...}`.
    Never raises.
    """
    session = f"trellis-quota-claude-{uuid.uuid4().hex[:8]}"
    cwd: Optional[Path] = None
    try:
        cwd = _make_throwaway_cwd("trellis-quota-claude-")
        # Post-bwrap-only: no sudo wrap. The supervisor invokes `claude`
        # from its own PATH; provider auth comes from `burst_home/.claude`
        # if set, else the supervisor's `~/.claude`.
        cmd: List[str] = ["claude"]
        env_prefix: List[str] = []
        if burst_home is not None:
            env_prefix = ["env", f"HOME={burst_home}", f"PATH={worker_path_env(burst_home)}"]
        spawn = subprocess.run(
            tmux_argv("new-session", "-d", "-s", session,
                      "-x", "220", "-y", "60", "-c", str(cwd),
                      *env_prefix, *cmd),
            capture_output=True, text=True, timeout=10, check=False,
        )
        if spawn.returncode != 0:
            return {"provider": "claude", "ok": False,
                    "error": f"tmux new-session failed: {spawn.stderr.strip()}"}
        # Wait for claude to render its welcome screen — the input prompt
        # only accepts slash commands once the TUI is past the splash.
        # Claude shows a per-folder trust dialog on fresh cwds now
        # ("Is this a project you created or one you trust?"). Dismiss
        # it with Enter (option 1 = Yes, trust). Then wait for the
        # welcome screen / input prompt to render.
        trust_dismissed = {"done": False}
        def _maybe_dismiss(cap: str) -> None:
            if trust_dismissed["done"]:
                return
            if "Is this a project" in cap or "1. Yes, I trust this folder" in cap:
                _tmux_send(session, "Enter")
                trust_dismissed["done"] = True
        ready = _wait_for(
            session,
            matches_any=(
                "bypass permissions on", "Welcome back",
                "/effort", "Run /init", "Recent activity",
            ),
            timeout=min(45.0, timeout_seconds * 0.6),
            poll_interval=1.0,
            side_effect_each_poll=_maybe_dismiss,
        )
        if ready is None:
            return {"provider": "claude", "ok": False, "error": "claude never reached ready state"}
        # Send /usage (whole literal, then Enter).
        _tmux_send(session, "C-u")
        time.sleep(0.4)
        _tmux_send(session, "/usage")
        time.sleep(0.4)
        _tmux_send(session, "Enter")
        rendered = _wait_for(
            session,
            matches_any=("Current week", "Resets"),
            timeout=min(20.0, timeout_seconds * 0.4),
            poll_interval=0.5,
        )
        if rendered is None:
            return {"provider": "claude", "ok": False, "error": "/usage never rendered"}
        time.sleep(1.5)
        final = _tmux_capture(session)
        return _parse_claude_usage(final)
    except Exception as e:
        return {
            "provider": "claude", "ok": False,
            "error": f"unexpected: {e!r}",
            "trace": traceback.format_exc(limit=4),
        }
    finally:
        try:
            _tmux_kill(session)
        except Exception:
            pass
        if cwd is not None:
            try:
                import shutil
                shutil.rmtree(cwd, ignore_errors=True)
            except Exception:
                pass


# `/usage` lines we care about (in pane-rendered form):
#   "Current session"          (5-hour rolling window)
#   "  ███...    11% used"
#   "  Resets 11pm (America/New_York)"
#   "Current week (all models)"  ← THE ONE THAT MATTERS for monthly cap planning
#   "  ███...    7% used"
#   "  Resets Apr 28, 3am (America/New_York)"
#   "Current week (Sonnet only)"  ← optional per-model variant
#
# We extract the % from any line containing "% used" that follows a
# section header, and the reset timestamp from the next "Resets ..." line.
_CLAUDE_USAGE_PCT_LINE = re.compile(r"(\d+(?:\.\d+)?)\s*%\s*used", re.IGNORECASE)
_CLAUDE_USAGE_RESETS_LINE = re.compile(r"Resets\s+(.+?)(?:\s*\(([^)]+)\))?\s*$", re.IGNORECASE)


def _parse_claude_usage(text: str) -> Dict[str, Any]:
    out: Dict[str, Any] = {"provider": "claude", "ok": True}
    lines = [ln.rstrip() for ln in text.splitlines()]
    # Walk the lines, recognizing section headers and their (pct, resets) pairs.
    sections: List[Tuple[str, Optional[float], Optional[str], Optional[str]]] = []
    current_header: Optional[str] = None
    current_pct: Optional[float] = None
    for ln in lines:
        s = ln.strip()
        # Section headers we care about.
        if s == "Current session" or s.startswith("Current session "):
            if current_header is not None:
                sections.append((current_header, current_pct, None, None))
            current_header = "five_hour"
            current_pct = None
            continue
        if s.startswith("Current week"):
            if current_header is not None:
                sections.append((current_header, current_pct, None, None))
            # Distinguish "all models" vs per-model variants (e.g. "Sonnet only").
            if "all models" in s.lower():
                current_header = "weekly"
            else:
                m = re.match(r"Current week \(([^)]+)\)", s)
                label = (m.group(1) if m else "model").strip().lower().replace(' ', '_')
                current_header = f"weekly_{label}"
            current_pct = None
            continue
        if current_header is None:
            continue
        m = _CLAUDE_USAGE_PCT_LINE.search(s)
        if m:
            try:
                current_pct = float(m.group(1))
            except Exception:
                current_pct = None
            continue
        m = _CLAUDE_USAGE_RESETS_LINE.match(s)
        if m:
            sections.append((current_header, current_pct, m.group(1).strip(), m.group(2)))
            current_header = None
            current_pct = None
            continue
    # Flush the trailing section if no Resets line followed it.
    if current_header is not None:
        sections.append((current_header, current_pct, None, None))

    now = time.time()
    windows: List[Dict[str, Any]] = []
    for (name, pct, resets_repr, tz) in sections:
        if name == "five_hour":
            window_seconds = 5 * 3600
        elif name == "weekly" or name.startswith("weekly_"):
            window_seconds = 7 * 24 * 3600
        else:
            window_seconds = 0
        resets_at = _parse_claude_reset_to_epoch(resets_repr, tz_name=tz, now=now) if resets_repr else None
        resets_in = (resets_at - int(now)) if resets_at else None
        frac = _frac_of_month(window_seconds) if window_seconds else None
        monthly_burn = round((pct or 0.0) * frac, 4) if (frac is not None and pct is not None) else None
        windows.append({
            "name": name,
            "pct_used": pct,
            "pct_used_kind": "exact",
            "resets_at": resets_at,
            "resets_at_repr": resets_repr,
            "resets_in_seconds": resets_in,
            "window_seconds": window_seconds or None,
            "fraction_of_month": round(frac, 6) if frac is not None else None,
            "monthly_burn_pct": monthly_burn,
        })
    out["windows"] = windows
    if not windows:
        out["ok"] = False
        out["error"] = "no quota windows parsed from /usage"
    return out


def _parse_claude_reset_to_epoch(repr_str: Optional[str], *, tz_name: Optional[str], now: float) -> Optional[int]:
    """Parse claude /usage reset strings:
      - "11pm"                 — same-day local time (or next day if past)
      - "Apr 28, 3am"          — next occurrence of Apr 28 at 03:00
    `tz_name` (e.g. "America/New_York") is the IANA tz claude reports;
    if zoneinfo can resolve it we use it, otherwise we fall back to local.
    Returns Unix epoch seconds, or None on parse failure.
    """
    if not repr_str:
        return None
    s = repr_str.strip().rstrip(',').strip()
    try:
        from zoneinfo import ZoneInfo
        tz = ZoneInfo(tz_name) if tz_name else None
    except Exception:
        tz = None
    import datetime
    now_dt = datetime.datetime.fromtimestamp(now, tz=tz) if tz else datetime.datetime.fromtimestamp(now)
    # "Apr 28, 3am" or "Apr 28, 3:30am"
    m = re.match(r"^([A-Za-z]{3,})\s+(\d{1,2}),\s+(\d{1,2})(?::(\d{2}))?\s*([AaPp][Mm])$", s)
    if m:
        mon_name, dd, hh, mm, ampm = m.groups()
        month_idx = _MONTH_NAME_TO_IDX.get(mon_name[:3].lower())
        if not month_idx:
            return None
        hh_i = int(hh) % 12 + (12 if ampm.lower() == 'pm' else 0)
        mm_i = int(mm) if mm else 0
        try:
            year = now_dt.year
            target = datetime.datetime(year, month_idx, int(dd), hh_i, mm_i, tzinfo=now_dt.tzinfo)
            if target < now_dt:
                target = target.replace(year=year + 1)
            return int(target.timestamp())
        except Exception:
            return None
    # "11pm" or "11:30am"
    m = re.match(r"^(\d{1,2})(?::(\d{2}))?\s*([AaPp][Mm])$", s)
    if m:
        hh, mm, ampm = m.groups()
        hh_i = int(hh) % 12 + (12 if ampm.lower() == 'pm' else 0)
        mm_i = int(mm) if mm else 0
        try:
            target = now_dt.replace(hour=hh_i, minute=mm_i, second=0, microsecond=0)
            if target <= now_dt:
                target += datetime.timedelta(days=1)
            return int(target.timestamp())
        except Exception:
            return None
    return None


# ----- shared tmux probe scaffolding ---------------------------------------

def _make_throwaway_cwd(prefix: str) -> Path:
    cwd = Path(tempfile.mkdtemp(prefix=prefix))
    # Some CLIs refuse non-git directories. Quietly init.
    subprocess.run(
        ["git", "-C", str(cwd), "init", "-q"],
        capture_output=True, timeout=10, check=False,
    )
    return cwd


def _tmux_capture(session: str, *, scrollback: int = 0) -> str:
    """Capture-pane. scrollback=0 (default) grabs only the live screen —
    correct for TUIs that use the alt-screen (codex/gemini), where the
    scrollback buffer stays empty no matter how far we ask back. Pass
    scrollback>0 only for normal-screen pseudo-terminals.
    """
    args = ["capture-pane", "-t", session, "-p"]
    if scrollback > 0:
        args += ["-S", f"-{scrollback}"]
    proc = subprocess.run(
        tmux_argv(*args),
        capture_output=True, text=True, timeout=8, check=False,
    )
    return proc.stdout or ""


def _tmux_send(session: str, *keys: str) -> None:
    subprocess.run(
        tmux_argv("send-keys", "-t", session, *keys),
        capture_output=True, timeout=8, check=False,
    )


def _tmux_kill(session: str) -> None:
    subprocess.run(
        tmux_argv("kill-session", "-t", session),
        capture_output=True, timeout=8, check=False,
    )


def _wait_for(
    session: str,
    matches_any: Tuple[str, ...],
    *,
    timeout: float,
    poll_interval: float = 1.0,
    side_effect_each_poll: Optional[Any] = None,
) -> Optional[str]:
    """Poll capture-pane until one of `matches_any` substrings appears.
    Returns the matching pane text, or None on timeout.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        cap = _tmux_capture(session)
        if any(m in cap for m in matches_any):
            return cap
        if side_effect_each_poll is not None:
            try:
                side_effect_each_poll(cap)
            except Exception:
                pass
        time.sleep(poll_interval)
    return None


# ----- codex probe (~15-20s, $0) -------------------------------------------

# "5h limit: [...] 99% left (resets 18:47)"
# "Weekly limit: [...] 1% left (resets 19:26 on 28 Apr)"
_CODEX_LIMIT_LINE = re.compile(
    r"(5h|Weekly)\s+limit:\s+\S*\s+(\d+)%\s+left\s+\(resets\s+([^)]+)\)",
    re.IGNORECASE,
)
_CODEX_ACCOUNT_LINE = re.compile(r"Account:\s+(\S+@\S+)\s*\(([^)]+)\)")
_CODEX_CREDITS_LINE = re.compile(r"Credits:\s+(\d+)")


def probe_codex(*, timeout_seconds: float = 60.0) -> Dict[str, Any]:
    """Spawn codex in a throwaway tmux session, send `/status`, parse pane.

    Reports only the cross-model `Weekly limit:` and `5h limit:` lines —
    per Q6, per-model breakdowns are intentionally skipped.

    Returns a normalized payload. On any failure: `{"ok": False, "error": ...}`.
    """
    session = f"trellis-quota-codex-{uuid.uuid4().hex[:8]}"
    cwd: Optional[Path] = None
    try:
        cwd = _make_throwaway_cwd("trellis-quota-codex-")
        spawn = subprocess.run(
            tmux_argv("new-session", "-d", "-s", session,
                      "-x", "220", "-y", "60", "-c", str(cwd), "codex"),
            capture_output=True, text=True, timeout=10, check=False,
        )
        if spawn.returncode != 0:
            return {"provider": "codex", "ok": False,
                    "error": f"tmux new-session failed: {spawn.stderr.strip()}"}
        # Dismiss the trust dialog (option 1 = Yes, continue) and the
        # "Update available!" version-prompt dialog if either appears.
        # The update dialog blocks the ready prompt indefinitely;
        # we pick option 3 ("Skip until next version") which persists
        # in ~/.codex state, so subsequent probes don't re-encounter
        # the same dialog. We deliberately do NOT pick option 1 here:
        # mid-run npm installs are disruptive and `restart_configured_run.sh`
        # is the operator-initiated path for fresh installs.
        trust_dismissed = {"done": False}
        update_dismissed = {"done": False}

        def _maybe_dismiss(cap: str) -> None:
            if not trust_dismissed["done"] and (
                "Yes, continue" in cap or "Do you trust" in cap
            ):
                _tmux_send(session, "Enter")
                trust_dismissed["done"] = True
            if not update_dismissed["done"] and "Update available" in cap:
                _tmux_send(session, "Down", "Down", "Enter")
                update_dismissed["done"] = True

        ready = _wait_for(
            session,
            matches_any=("/model to change", "Tip: Use /compact"),
            timeout=min(45.0, timeout_seconds * 0.6),
            side_effect_each_poll=_maybe_dismiss,
        )
        if ready is None:
            return {"provider": "codex", "ok": False, "error": "codex never reached ready state"}
        # Send /status as a single literal string + Enter. Splitting it as
        # `/` + `status` would open the slash-menu's fuzzy search and then
        # type into it — codex's /status entry doesn't always autocomplete
        # the way you'd expect, and sometimes another command (e.g. /model)
        # gets selected instead. Sending the whole command verbatim avoids
        # the menu interaction entirely.
        _tmux_send(session, "C-u")
        time.sleep(0.4)
        _tmux_send(session, "/status")
        time.sleep(0.4)
        _tmux_send(session, "Enter")
        # Wait for the panel to render. capture-pane (no -S) reads the
        # live alt-screen — scrollback is empty for TUIs.
        rendered = _wait_for(
            session,
            matches_any=("5h limit:", "Weekly limit:"),
            timeout=min(25.0, timeout_seconds * 0.4),
            poll_interval=0.5,
        )
        if rendered is None:
            return {"provider": "codex", "ok": False, "error": "/status never rendered"}
        return _parse_codex_status(rendered)
    except Exception as e:
        return {
            "provider": "codex", "ok": False,
            "error": f"unexpected: {e!r}",
            "trace": traceback.format_exc(limit=4),
        }
    finally:
        try:
            _tmux_kill(session)
        except Exception:
            pass
        if cwd is not None:
            try:
                import shutil
                shutil.rmtree(cwd, ignore_errors=True)
            except Exception:
                pass


def _parse_codex_status(text: str) -> Dict[str, Any]:
    out: Dict[str, Any] = {"provider": "codex", "ok": True}
    windows: List[Dict[str, Any]] = []
    seen_default = False
    now = time.time()
    for raw in text.splitlines():
        s = raw.strip().lstrip("│ ").rstrip(" │").strip()
        m = _CODEX_ACCOUNT_LINE.search(s)
        if m:
            out["account"] = m.group(1)
            out["plan_tier"] = m.group(2)
            continue
        m = _CODEX_CREDITS_LINE.search(s)
        if m:
            out["credits"] = int(m.group(1))
            continue
        m = _CODEX_LIMIT_LINE.search(s)
        if m and not seen_default:
            kind = m.group(1).lower()
            pct_left = int(m.group(2))
            resets_repr = m.group(3).strip()
            window_name = "five_hour" if kind == "5h" else "weekly"
            window_seconds = 5 * 3600 if kind == "5h" else 7 * 24 * 3600
            resets_at = _parse_codex_reset_to_epoch(resets_repr, now=now)
            resets_in = (resets_at - int(now)) if resets_at else None
            pct_used = max(0, min(100, 100 - pct_left))
            frac = _frac_of_month(window_seconds)
            windows.append({
                "name": window_name,
                "pct_used": float(pct_used),
                "pct_used_kind": "exact",
                "resets_at": resets_at,
                "resets_at_repr": resets_repr,
                "resets_in_seconds": resets_in,
                "window_seconds": window_seconds,
                "fraction_of_month": round(frac, 6),
                "monthly_burn_pct": round(pct_used * frac, 4),
            })
            if window_name == "weekly":
                seen_default = True
    out["windows"] = windows
    if not windows:
        out["ok"] = False
        out["error"] = "no 5h/Weekly limit lines parsed"
    return out


def _parse_codex_reset_to_epoch(repr_str: str, *, now: float) -> Optional[int]:
    """Parse codex's reset-time strings:
      - "18:47"             — same-day HH:MM (or next day if past)
      - "19:26 on 28 Apr"   — HH:MM on DD Mon (current year, or next year if past)
    Returns Unix epoch seconds, or None on parse failure.
    """
    s = repr_str.strip()
    # "HH:MM on DD MMM"
    m = re.match(r"^(\d{1,2}):(\d{2})\s+on\s+(\d{1,2})\s+(\w{3,})$", s)
    if m:
        hh, mm, dd, mon = m.groups()
        try:
            import datetime
            now_dt = datetime.datetime.fromtimestamp(now)
            month_idx = _MONTH_NAME_TO_IDX.get(mon[:3].lower())
            if not month_idx:
                return None
            year = now_dt.year
            target = datetime.datetime(year, month_idx, int(dd), int(hh), int(mm))
            if target < now_dt:
                target = target.replace(year=year + 1)
            return int(target.timestamp())
        except Exception:
            return None
    # "HH:MM"
    m = re.match(r"^(\d{1,2}):(\d{2})$", s)
    if m:
        hh, mm = m.groups()
        try:
            import datetime
            now_dt = datetime.datetime.fromtimestamp(now)
            target = now_dt.replace(hour=int(hh), minute=int(mm), second=0, microsecond=0)
            if target <= now_dt:
                target += datetime.timedelta(days=1)
            return int(target.timestamp())
        except Exception:
            return None
    return None


_MONTH_NAME_TO_IDX = {
    "jan": 1, "feb": 2, "mar": 3, "apr": 4, "may": 5, "jun": 6,
    "jul": 7, "aug": 8, "sep": 9, "oct": 10, "nov": 11, "dec": 12,
}


# ----- gemini probe (~15-20s, $0) ------------------------------------------

# Per-model-category usage line shape (from `gemini /model`):
#   "Flash      ▬▬...▬▬ 2%   Resets: 3:12 PM (20m)"
#   "Flash Lite ▬▬...▬▬ 0%   Resets: 6:54 PM (4h 1m)"
#   "Pro        ▬▬...▬▬ 3%   Resets: 1:14 PM (22h 21m)"
# The bar segment is ASCII art; we ignore it and pluck the tail.
# Model categories observed in gemini-cli 0.39.0; new ones can appear
# without breaking the parse — only "Resets:" presence matters.
_GEMINI_MODEL_LINE = re.compile(
    r"^\s*(Flash Lite|Flash|Pro)\s+\S.*?(\d+)%\s+Resets:\s+(\d{1,2}:\d{2}\s*[AP]M)\s+\(([^)]+)\)\s*$"
)
_GEMINI_TIER_LINE = re.compile(r"Tier:\s+(.+)$")
_GEMINI_AUTH_LINE = re.compile(r"Auth Method:.*?\(([^)]+)\)")


def probe_gemini(
    *,
    burst_home: Optional[Path] = None,
    timeout_seconds: float = 60.0,
) -> Dict[str, Any]:
    """Spawn gemini in a throwaway tmux session, send `/model`, parse pane.

    `/model` shows the per-model-category quota table (Flash, Flash Lite,
    Pro), each with its own pct_used + reset cadence (per Q3 the cadences
    differ per category — typically minutes for Flash, hours for Flash
    Lite, ~daily for Pro). `/stats` was the original target but its body
    in gemini-cli 0.39.0 doesn't include reset times anymore — they moved
    to `/model`'s "Model usage" section.

    On any failure: `{"ok": False, "error": ...}`.
    """
    session = f"trellis-quota-gemini-{uuid.uuid4().hex[:8]}"
    cwd: Optional[Path] = None
    try:
        cwd = _make_throwaway_cwd("trellis-quota-gemini-")
        # Post-bwrap-only: no sudo wrap. Honor an explicit `burst_home` if
        # provided (the bridge passes the per-burst fake-home) so the
        # gemini install under `<burst_home>/.trellis-npm/bin` takes
        # precedence over the older system `/usr/bin/gemini` (which lacks
        # the `/model` quota table).
        cmd: List[str] = ["gemini"]
        env_prefix: List[str] = []
        if burst_home is not None:
            path = f"{burst_home}/.trellis-npm/bin:/usr/local/bin:/usr/bin:/bin"
            env_prefix = ["env", f"HOME={burst_home}", f"PATH={path}"]
        spawn = subprocess.run(
            tmux_argv("new-session", "-d", "-s", session,
                      "-x", "220", "-y", "60", "-c", str(cwd),
                      *env_prefix, *cmd),
            capture_output=True, text=True, timeout=10, check=False,
        )
        if spawn.returncode != 0:
            return {"provider": "gemini", "ok": False,
                    "error": f"tmux new-session failed: {spawn.stderr.strip()}"}
        trust_dismissed = {"done": False}

        def _maybe_dismiss(cap: str) -> None:
            if not trust_dismissed["done"] and (
                "Trust folder" in cap or "Don't trust" in cap or "Do you trust" in cap
            ):
                _tmux_send(session, "1", "Enter")
                trust_dismissed["done"] = True

        # Match on `Type your message` only — it appears in the prompt
        # box AFTER the trust dialog is dismissed. The "Auto (Gemini ...)"
        # footer is rendered even while the trust dialog is still showing,
        # so matching on that would race past the dismissal step.
        ready = _wait_for(
            session,
            matches_any=("Type your message",),
            timeout=min(45.0, timeout_seconds * 0.6),
            side_effect_each_poll=_maybe_dismiss,
        )
        if ready is None:
            return {"provider": "gemini", "ok": False, "error": "gemini never reached ready state"}
        _tmux_send(session, "C-u")
        time.sleep(0.4)
        _tmux_send(session, "/model")
        time.sleep(0.4)
        _tmux_send(session, "Enter")
        rendered = _wait_for(
            session,
            matches_any=("Model usage", "Resets:"),
            timeout=min(25.0, timeout_seconds * 0.4),
            poll_interval=0.5,
        )
        if rendered is None:
            return {"provider": "gemini", "ok": False, "error": "/model never rendered"}
        # Give the table a moment to fully render before final capture.
        time.sleep(2.0)
        final = _tmux_capture(session)
        # Press Esc to dismiss the model picker before tearing down — keeps
        # the gemini child cleanly exitable.
        _tmux_send(session, "Escape")
        return _parse_gemini_stats(final)
    except Exception as e:
        return {
            "provider": "gemini", "ok": False,
            "error": f"unexpected: {e!r}",
            "trace": traceback.format_exc(limit=4),
        }
    finally:
        try:
            _tmux_kill(session)
        except Exception:
            pass
        if cwd is not None:
            try:
                import shutil
                shutil.rmtree(cwd, ignore_errors=True)
            except Exception:
                pass


def _parse_gemini_duration_to_seconds(s: str) -> Optional[int]:
    """Parse '46m', '4h 27m', '22h 47m', '1d 3h 10m' → seconds."""
    total = 0
    matched = False
    for n, unit in re.findall(r"(\d+)\s*([dhms])", s):
        try:
            v = int(n)
        except Exception:
            continue
        if unit == "d":
            total += v * 86400
        elif unit == "h":
            total += v * 3600
        elif unit == "m":
            total += v * 60
        elif unit == "s":
            total += v
        matched = True
    return total if matched else None


def _parse_gemini_stats(text: str) -> Dict[str, Any]:
    out: Dict[str, Any] = {"provider": "gemini", "ok": True}
    models: List[Dict[str, Any]] = []
    now = time.time()
    for raw in text.splitlines():
        s = raw.strip().lstrip("│ ").rstrip(" │").strip()
        m = _GEMINI_TIER_LINE.search(s)
        if m:
            out["plan_tier"] = m.group(1).strip()
            continue
        m = _GEMINI_AUTH_LINE.search(s)
        if m:
            out["account"] = m.group(1)
            continue
        m = _GEMINI_MODEL_LINE.match(s)
        if m:
            category = m.group(1)
            try:
                pct_used = float(m.group(2))
            except ValueError:
                continue
            resets_at_repr = m.group(3).strip()
            resets_in_repr = m.group(4).strip()
            resets_in = _parse_gemini_duration_to_seconds(resets_in_repr)
            resets_at = (int(now) + resets_in) if resets_in is not None else None
            # All gemini Code Assist model categories use a fixed 24-hour
            # quota window. The "resets in <X>" string is time-to-next-
            # rollover (which varies because each category's window
            # started at a different point in the day), NOT the window
            # length itself. Use 24h for the monthly-burn calculation so
            # the figure is comparable across providers.
            window_seconds = 24 * 3600
            frac = _frac_of_month(window_seconds)
            monthly_burn = round(pct_used * frac, 4)
            models.append({
                "category": category,
                "pct_used": pct_used,
                "pct_used_kind": "exact",
                "resets_at": resets_at,
                "resets_at_repr": resets_at_repr,
                "resets_in_repr": resets_in_repr,
                "resets_in_seconds": resets_in,
                "window_seconds": window_seconds or None,
                "fraction_of_month": round(frac, 6) if frac is not None else None,
                "monthly_burn_pct": monthly_burn,
            })
    out["models"] = models
    if not models:
        out["ok"] = False
        out["error"] = "no per-category usage rows parsed from /model output"
    return out


# ----- top-level gated probe ------------------------------------------------

def maybe_probe_provider(
    provider: str,
    repo: Path,
    *,
    burst_home: Optional[Path] = None,
    force: bool = False,
) -> Optional[Dict[str, Any]]:
    """Cooldown- and circuit-breaker-gated probe.

    Returns the probe payload if a probe was actually run, else None.
    On any internal failure, logs a row to the snapshots ledger and returns
    that error payload (still non-None so the caller knows we tried).

    NEVER raises. Quota tracking is cosmetic — a crash here must not bring
    down the supervisor. After PROBE_FAILURE_THRESHOLD consecutive failures
    a provider is suspended for the rest of the process.
    """
    try:
        if not force and not should_probe(provider, repo=repo):
            return None
        if provider == "claude":
            payload = probe_claude(burst_home=burst_home)
        elif provider == "codex":
            payload = probe_codex()
        elif provider == "gemini":
            payload = probe_gemini(burst_home=burst_home)
        else:
            return None
        mark_probed(provider)
        ok = bool(payload.get("ok"))
        _record_probe_outcome(provider, ok)
        if not ok and provider in _SUSPENDED_PROVIDERS:
            payload = dict(payload)
            payload["suspended"] = True
            payload["suspended_reason"] = (
                f"{PROBE_FAILURE_THRESHOLD} consecutive failures; no further probes this run"
            )
        try:
            append_quota_snapshot(repo, payload)
        except Exception:
            pass
        return payload
    except Exception as e:
        # Final safety net: NEVER let a quota probe break a run.
        try:
            _record_probe_outcome(provider, False)
        except Exception:
            pass
        try:
            append_quota_snapshot(
                repo,
                {"provider": provider, "ok": False,
                 "error": f"maybe_probe_provider crash: {e!r}",
                 "trace": traceback.format_exc(limit=4)},
            )
        except Exception:
            pass
        return {"provider": provider, "ok": False, "error": str(e)}


def maybe_probe_at_burst_boundary(
    provider: str,
    repo: Path,
    *,
    burst_home: Optional[Path] = None,
) -> None:
    """Convenience wrapper for the burst start/end hook.

    Called immediately before dispatching an agent burst, and immediately
    after the cost-ledger row is written. Both calls go through the same
    cooldown gate; in practice one or both fire only when ≥30 min has
    elapsed since the prior probe for this provider.

    NEVER raises. Errors are logged to the snapshots ledger.
    """
    try:
        maybe_probe_provider(
            provider, repo, burst_home=burst_home,
        )
    except Exception:
        pass


# ----- forced burst-bracketing probe (NOT cooldown-gated) ------------------
#
# These probes drive per-burst USD attribution: pre-burst snapshot + post-burst
# snapshot give a within-burst delta in pct_used, which (via the long-cadence
# window) maps to a USD figure for the burst. The 30-min cooldown that gates
# `maybe_probe_provider` is intentionally bypassed here — for attribution we
# need fresh snapshots at every burst boundary.
#
# Two safeguards keep this from runaway:
#   - per-provider lock + 5s dedup cache: concurrent panel members of the same
#     provider would otherwise launch redundant probes; account state cannot
#     have changed meaningfully in 5s, so the second caller reuses the first
#     payload.
#   - the existing circuit breaker (`_SUSPENDED_PROVIDERS`) still applies.
#     3 consecutive failures suspends probing for that provider this run.

_PROBE_LOCKS: Dict[str, threading.Lock] = defaultdict(threading.Lock)
_LAST_PROBE_PAYLOAD: Dict[str, Tuple[float, Dict[str, Any]]] = {}
_PROBE_DEDUP_WINDOW_SECONDS = 5.0


def probe_for_burst(
    provider: str,
    repo: Path,
    *,
    burst_home: Optional[Path] = None,
) -> Optional[Dict[str, Any]]:
    """Forced (non-cooldown-gated) probe for per-burst attribution.

    Same payload shape as `maybe_probe_provider`. Returns None when the
    provider is suspended or a hard error escapes the inner probe.

    NEVER raises. Quota tracking is cosmetic and must not break a run.
    """
    try:
        with _PROBE_LOCKS[provider]:
            cached = _LAST_PROBE_PAYLOAD.get(provider)
            if cached and (time.time() - cached[0]) < _PROBE_DEDUP_WINDOW_SECONDS:
                return cached[1]
            if provider in _SUSPENDED_PROVIDERS:
                return None
            if provider == "claude":
                payload = probe_claude(burst_home=burst_home)
            elif provider == "codex":
                payload = probe_codex()
            elif provider == "gemini":
                payload = probe_gemini(burst_home=burst_home)
            else:
                return None
            ok = bool(payload.get("ok"))
            try:
                _record_probe_outcome(provider, ok)
            except Exception:
                pass
            _LAST_PROBE_PAYLOAD[provider] = (time.time(), payload)
            try:
                append_quota_snapshot(repo, payload)
            except Exception:
                pass
            return payload
    except Exception:
        try:
            _record_probe_outcome(provider, False)
        except Exception:
            pass
        return None


def quota_projection_for_ledger(
    payload: Optional[Dict[str, Any]],
    provider: str,
) -> Optional[Dict[str, Any]]:
    """Compact projection of a probe payload for stamping on a cost-ledger row.

    Returns None if `payload` is None or `payload.ok` is not truthy. The full
    snapshot already lives in `quota-snapshots.jsonl`; the cost-ledger row
    only needs the few fields that drive per-burst USD attribution.

    Shape:
      claude/codex: {five_hour_pct, five_hour_resets_at,
                     weekly_pct, weekly_resets_at, ts, ok}
      gemini:       {models: [{category, pct_used, resets_at}, ...], ts, ok}

    Never raises.
    """
    try:
        if payload is None or not payload.get("ok"):
            return None
        ts = payload.get("ts") or time.time()
        if provider in ("claude", "codex"):
            five_pct: Optional[float] = None
            five_resets: Optional[int] = None
            week_pct: Optional[float] = None
            week_resets: Optional[int] = None
            for w in payload.get("windows") or []:
                name = w.get("name")
                if name == "five_hour":
                    five_pct = w.get("pct_used")
                    five_resets = w.get("resets_at")
                elif name == "weekly":
                    week_pct = w.get("pct_used")
                    week_resets = w.get("resets_at")
            return {
                "five_hour_pct": five_pct,
                "five_hour_resets_at": five_resets,
                "weekly_pct": week_pct,
                "weekly_resets_at": week_resets,
                "ts": ts,
                "ok": True,
            }
        if provider == "gemini":
            models = []
            for m in payload.get("models") or []:
                models.append({
                    "category": m.get("category"),
                    "pct_used": m.get("pct_used"),
                    "resets_at": m.get("resets_at"),
                })
            return {"models": models, "ts": ts, "ok": True}
        return None
    except Exception:
        return None
