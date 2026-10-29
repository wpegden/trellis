"""Antigravity CLI (`agy`) provider helpers.

`agy` is the Antigravity standalone Go binary (`~/.local/bin/agy`) that drives
Gemini 3.1 Pro (and other models) through an interactive TUI. Trellis runs it
in a tmux pane inside the same bwrap sandbox used for the gemini-cli, claude,
and codex providers, and drives it by pane-scraping.

This module holds the agy-specific bits that differ from gemini-cli:
  - settings seeding (simplified TUI for reliable pane-scraping)
  - conversation-id discovery for cross-burst resume (`--conversation <id>`)
  - idle-input-box detection (agy renders a bare `>` prompt, not gemini's
    "Type your message" placeholder)
  - last-assistant extraction from the pane (agy's conversation store is an
    opaque protobuf SQLite blob, so the pane is the practical transcript)

The shared pane-scrape plumbing (busy markers, spinner glyphs, normalize_pane,
settle/idle loops) lives in tmux_backend.py and treats `antigravity` like
`gemini` where the rendering is identical.

Canonical model string (agy accepts the display name verbatim):
    "Gemini 3.1 Pro (High)"
"""

from __future__ import annotations

import json
import os
import re
from pathlib import Path
from typing import List, Optional

# The agy CLI command name. Resolved on PATH (installed at ~/.local/bin/agy);
# the sandbox binds its directory read-only via host_runtime "agy" tool.
AGY_COMMAND = "agy"

# Canonical worker model. agy's `--model` accepts the display name verbatim
# (verified: `agy --model "Gemini 3.1 Pro (High)" -p ...` and the value also
# round-trips through ~/.gemini/antigravity-cli/settings.json["model"]).
DEFAULT_MODEL = "Gemini 3.1 Pro (High)"

# agy stores its state (oauth token, conversations, history, settings) under
# ~/.gemini/antigravity-cli/, which is already inside the ~/.gemini tree the
# bursts bind-mount.
def antigravity_state_dir(home: Path) -> Path:
    return home / ".gemini" / "antigravity-cli"


def antigravity_settings_path(home: Path) -> Path:
    return antigravity_state_dir(home) / "settings.json"


def antigravity_history_path(home: Path) -> Path:
    return antigravity_state_dir(home) / "history.jsonl"


# Settings that make agy's TUI maximally pane-scrape friendly. Seeded into the
# burst HOME's settings.json before launch (merged over whatever is there).
#
#   altScreenMode=never    -> keep output in the normal scrollback (linear,
#                             capture-pane sees a stable transcript) instead of
#                             a redrawn alternate screen buffer.
#   runningLightSpeed=off   -> drop artificial typing delays / thought-stream
#                             animation. With verbosity=low this removes the
#                             collapsible "Thought for Ns, N tokens /
#                             Prioritizing Tool Usage" block, so the assistant
#                             message renders directly under the user turn.
#   verbosity=low           -> minimal agent-trace rendering.
#   toolPermission=always-proceed -> pairs with --dangerously-skip-permissions
#                             so no tool-confirmation prompt can wedge the pane.
#   showTips / showFeedbackSurvey / notifications=false -> remove extraneous UI
#                             that would add noise to the scraped pane.
#   colorScheme=terminal    -> default; named explicitly so a stray user
#                             color scheme can't inject ANSI we don't strip.
_TUI_SETTINGS = {
    "altScreenMode": "never",
    "runningLightSpeed": "off",
    "verbosity": "low",
    "toolPermission": "always-proceed",
    "showTips": False,
    "showFeedbackSurvey": False,
    "notifications": False,
    "colorScheme": "terminal",
    "enableTelemetry": False,
}


def seed_antigravity_settings(
    home: Path,
    *,
    model: Optional[str] = None,
) -> None:
    """Merge the pane-scrape-friendly TUI settings into <home> settings.json.

    Idempotent. Preserves any pre-existing keys (e.g. an operator-set model)
    that we don't explicitly override. The burst HOME is supervisor-owned, so
    this is a direct write (no sudo).
    """
    path = antigravity_settings_path(home)
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(data, dict):
            data = {}
    except Exception:
        data = {}
    data.update(_TUI_SETTINGS)
    if model:
        data["model"] = model
    tmp = path.with_suffix(f".json.tmp.{os.getpid()}")
    tmp.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    try:
        tmp.chmod(0o600)
    except OSError:
        pass
    os.replace(tmp, path)


def latest_conversation_id(
    work_dir: Path,
    *,
    home: Path,
) -> Optional[str]:
    """Most recent agy conversationId for `work_dir`, for `--conversation` resume.

    agy appends one JSONL record per turn to
    ~/.gemini/antigravity-cli/history.jsonl, each carrying the `workspace`
    (absolute cwd) and the `conversationId`. We resume the conversation whose
    workspace matches our burst cwd so concurrent lanes never cross-resume.

    Returns None when there's no prior conversation for this workspace (the
    caller then launches fresh).
    """
    path = antigravity_history_path(home)
    try:
        text = path.read_text(encoding="utf-8")
    except (FileNotFoundError, PermissionError, OSError):
        return None
    target = str(work_dir.resolve())
    found: Optional[str] = None
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(rec, dict):
            continue
        if str(rec.get("workspace", "")) != target:
            continue
        cid = rec.get("conversationId")
        if isinstance(cid, str) and cid:
            found = cid  # keep the last (most recent) match
    return found


def conversation_db_path(work_dir: Path, *, home: Path) -> Optional[Path]:
    """SQLite db backing the latest conversation for `work_dir`, or None.

    Used purely as a liveness probe (its mtime advances as agy persists turn
    state) and as a transcript-path artifact pointer. Its contents are opaque
    protobuf, so we never parse it for message text.
    """
    cid = latest_conversation_id(work_dir, home=home)
    if not cid:
        return None
    p = antigravity_state_dir(home) / "conversations" / f"{cid}.db"
    return p if p.exists() else None


def latest_conversation_mtime_ns(work_dir: Path, *, home: Path) -> int:
    """mtime_ns of the active conversation db — positive work-signal probe.

    agy rewrites the conversation db as the turn streams, so its mtime is an
    unforgeable liveness signal (a TUI-render thread can't advance it). Falls
    back to the newest db in the conversations dir if the history lookup
    misses (e.g. first turn not yet flushed to history.jsonl). Returns 0 on
    any miss.
    """
    db = conversation_db_path(work_dir, home=home)
    if db is None:
        conv_dir = antigravity_state_dir(home) / "conversations"
        try:
            dbs = list(conv_dir.glob("*.db"))
        except OSError:
            return 0
        if not dbs:
            return 0
        db = max(dbs, key=lambda p: _safe_mtime_ns(p))
    return _safe_mtime_ns(db)


def _safe_mtime_ns(path: Path) -> int:
    try:
        return path.stat().st_mtime_ns
    except OSError:
        return 0


# ---- pane scraping ---------------------------------------------------------

# agy's idle input box renders as a line that is just `>` (optionally with a
# leading box bar). The status line reads "? for shortcuts ... <model>" when
# idle and "esc to cancel ... <model>" while generating.
_IDLE_STATUS = "? for shortcuts"
_PROMPT_LINE_RE = re.compile(r"^\s*[>❯]\s*$")


def input_box_is_empty(norm: str) -> bool:
    """True iff the agy pane is at an idle, empty input prompt.

    Criterion: the "? for shortcuts" idle status line is present AND the last
    meaningful line is a bare `>` prompt (nothing typed). This mirrors the
    gemini `_gemini_input_line_is_empty` contract used by settle/idle loops.
    """
    if _IDLE_STATUS not in norm:
        return False
    for line in reversed(norm.splitlines()):
        s = line.strip()
        if not s:
            continue
        # Skip the box separator rules and the status line itself.
        if s.startswith("──") or _IDLE_STATUS in s:
            continue
        return bool(_PROMPT_LINE_RE.match(line)) or line.strip() in (">", "❯")
    return False


# Lines that are pure TUI chrome (header banner, separators, status line) and
# should never be treated as assistant content.
_CHROME_PREFIXES = ("▄", "▀", "─", "?")


def last_assistant_message(pane_text: str) -> str:
    """Extract the most recent assistant reply from a scraped agy pane.

    agy renders each user turn on a line beginning `> ` and the model's reply
    as the indented (`  `) lines that follow, up to the next separator rule or
    the trailing idle input box. We return the text of the reply that follows
    the LAST user turn. The conversation store itself is opaque protobuf, so
    the pane is the practical transcript source; this feeds chat-history /
    viewer display only (worker acceptance reads the done_file artifact).
    """
    # Strip ANSI defensively (callers usually pass an already-stripped pane,
    # but be robust).
    lines = pane_text.splitlines()
    # Find the last user-turn marker: a line starting with "> " that has text.
    last_user_idx = -1
    for i, ln in enumerate(lines):
        st = ln.lstrip()
        if st.startswith("> ") and st[2:].strip():
            last_user_idx = i
    if last_user_idx < 0:
        return ""
    out: List[str] = []
    for ln in lines[last_user_idx + 1:]:
        st = ln.strip()
        if not st:
            # Preserve internal blank lines only once we've started collecting.
            if out:
                out.append("")
            continue
        if st.startswith("──"):
            break  # separator -> end of the assistant block / start of input
        if st.startswith("> "):
            break  # a newer user turn (shouldn't happen past the last, but safe)
        if st[0] in _CHROME_PREFIXES:
            continue  # banner / status chrome
        if st == ">" or st == "❯":
            break  # idle input box
        out.append(ln.strip())
    # Trim trailing blanks.
    while out and out[-1] == "":
        out.pop()
    return "\n".join(out).strip()


# agy startup dialogs. With --dangerously-skip-permissions and a pre-seeded
# trusted/authenticated state none of these are expected, but we dismiss them
# defensively the same way the gemini path does.
DIALOGS = [
    ("Do you trust the files in this folder", ("Enter",)),
    ("Yes, I trust this folder", ("Enter",)),
    ("Trust folder", ("Enter",)),
    ("Select Theme", ("Enter",)),
    ("Log in", ("Enter",)),  # shouldn't trigger; we're OAuth'd
]

# agy busy markers (in addition to the shared spinner glyphs). "Generating..."
# is the agy-specific in-flight label; "esc to cancel" is its busy status line
# (already covered by the gemini busy markers, kept here for clarity).
BUSY_MARKERS = (
    "generating...",
    "esc to cancel",
)

# agy auth-failure markers seen in the pane when the oauth token is rejected.
AUTH_MARKERS = (
    "Starting login flow",
    "Log in with Google",
    "authentication failed",
    "Please log in",
)
