"""Terminal monitor for an active trellis run.

Reads the same on-disk JSON the web viewer reads (no Node/HTTP), and renders
a live split-panel TUI: run/cycle/pipeline/nodes/contract stats on top,
streaming chat tail underneath.

Run as: python -m trellis.cli_monitor [--repo PATH] [--refresh SEC] [--once]
"""

from __future__ import annotations

import argparse
import json
import os
import select
import sys
import termios
import time
import tty
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

from rich.console import Console, Group
from rich.layout import Layout
from rich.live import Live
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

from trellis import chat_history, viewer_adapter


# ---------- repo discovery -------------------------------------------------


def _find_repo(start: Path) -> Path:
    cur = start.resolve()
    for _ in range(8):
        if (cur / ".trellis").is_dir():
            return cur
        if cur.parent == cur:
            break
        cur = cur.parent
    return start.resolve()


# ---------- helpers --------------------------------------------------------


def _fmt_age(epoch: Optional[float]) -> str:
    if not epoch:
        return "-"
    delta = max(0.0, time.time() - float(epoch))
    if delta < 60:
        return f"{delta:.0f}s ago"
    if delta < 3600:
        return f"{delta / 60:.0f}m ago"
    if delta < 86400:
        return f"{delta / 3600:.1f}h ago"
    return f"{delta / 86400:.1f}d ago"


def _safe_int(value: Any, default: int = 0) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def _read_runtime_metadata(repo: Path) -> Dict[str, Any]:
    info = viewer_adapter._load_runtime_info(repo)
    if info is None:
        return {}
    meta_path = info.root / "runtime_metadata.json"
    try:
        with meta_path.open() as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}


def _read_protocol_state(repo: Path) -> Dict[str, Any]:
    info = viewer_adapter._load_runtime_info(repo)
    if info is None:
        return {}
    try:
        with info.protocol_state_path.open() as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}


def _coarse_shallow_closed(
    coarse: List[str],
    present: List[str],
    open_nodes: List[str],
    deps: Dict[str, List[str]],
) -> int:
    """Port of viewer/server.js isCoarseShallowlyClosed; returns count over `coarse`.

    A coarse node N is shallow-closed iff N is kernel-closed (in present, not
    open) AND every non-coarse descendant of N (via deps) is kernel-closed.
    Descent stops at other coarse nodes (treated as opaque leaves).
    """
    present_set = set(present)
    open_set = set(open_nodes)
    coarse_set = set(coarse)

    def is_kernel_closed(n: str) -> bool:
        return n in present_set and n not in open_set

    memo: Dict[str, bool] = {}

    def shallowly_closed(n: str, stack: set) -> bool:
        if n in memo:
            return memo[n]
        if n in stack:
            return True  # cycle guard
        if not is_kernel_closed(n):
            memo[n] = False
            return False
        stack.add(n)
        for child in deps.get(n, []) or []:
            if child in coarse_set:
                continue  # opaque leaf
            if not shallowly_closed(child, stack):
                stack.discard(n)
                memo[n] = False
                return False
        stack.discard(n)
        memo[n] = True
        return True

    return sum(1 for n in coarse if shallowly_closed(n, set()))


def _closure_version(records: Dict[str, Any]) -> str:
    """Pull the closure_version stamp from the first record (all share it)."""
    for entry in records.values():
        if isinstance(entry, dict):
            v = entry.get("closure_version")
            if v:
                return str(v)
    return ""


# ---------- panel builders -------------------------------------------------


def _panel_run(repo: Path, runtime_info: Any, metadata: Dict[str, Any]) -> Panel:
    table = Table.grid(padding=(0, 1))
    table.add_column(style="dim", justify="right")
    table.add_column()
    table.add_row("repo", str(repo))
    if runtime_info is not None:
        table.add_row("runtime", str(runtime_info.root))
    contract = (
        metadata.get("contract_version")
        or metadata.get("contract")
        or metadata.get("_closure_version_hint")
        or "(unset)"
    )
    table.add_row("contract", str(contract))
    return Panel(table, title="Run", border_style="cyan")


def _panel_cycle(state: Dict[str, Any]) -> Panel:
    t = Table.grid(padding=(0, 1))
    t.add_column(style="dim", justify="right")
    t.add_column()
    t.add_row("cycle", str(state.get("cycle", "-")))
    t.add_row("phase", str(state.get("phase") or "-"))
    t.add_row("active", str(state.get("active_node") or "-") or "-")
    target = state.get("theorem_soundness_target") or "-"
    t.add_row("target", str(target) or "-")
    return Panel(t, title="Cycle", border_style="cyan")


def _panel_pipeline(view: Dict[str, Any]) -> Panel:
    state = view.get("state", {})
    meta = view.get("meta", {})
    t = Table.grid(padding=(0, 1))
    t.add_column(style="dim", justify="right")
    t.add_column()

    source = meta.get("source") or "-"
    t.add_row("source", str(source))

    in_flight = state.get("in_flight_analysis") or {}
    stall = in_flight.get("stall_state") or in_flight.get("state") or "-"
    elapsed = in_flight.get("elapsed_seconds")
    if isinstance(elapsed, (int, float)):
        t.add_row("stall", f"{stall} ({elapsed:.0f}s)")
    else:
        t.add_row("stall", str(stall))

    last_review = state.get("last_review") or {}
    decision = last_review.get("decision") or "-"
    reason = (last_review.get("reason") or "").strip().splitlines()[0:1]
    reason_txt = f' — "{reason[0][:60]}"' if reason else ""
    t.add_row("last review", f"{decision}{reason_txt}")

    last_worker = state.get("last_worker_handoff") or {}
    outcome = last_worker.get("outcome") or "-"
    summary = (last_worker.get("summary") or "").strip().splitlines()[0:1]
    sum_txt = f' — "{summary[0][:60]}"' if summary else ""
    t.add_row("last worker", f"{outcome}{sum_txt}")

    awaiting = "yes" if state.get("awaiting_human_input") else "no"
    t.add_row("awaiting human", awaiting)

    blockers = state.get("open_blockers") or []
    if blockers:
        first = str(blockers[0])[:80]
        t.add_row("open blockers", f"{len(blockers)} — {first}")
    else:
        t.add_row("open blockers", "0")

    return Panel(t, title="Pipeline", border_style="magenta")


def _panel_nodes(state: Dict[str, Any]) -> Panel:
    live = state.get("live", {}) or {}
    committed = state.get("committed", {}) or {}
    coverage = state.get("coverage", {}) or {}
    targets = state.get("configured_targets") or []
    covered = sum(1 for tgt in targets if coverage.get(str(tgt)))
    coarse = state.get("coarse_dag_nodes") or []
    protected = state.get("protected_reapproval_nodes") or []

    t = Table.grid(padding=(0, 1))
    t.add_column(style="dim", justify="right")
    t.add_column()
    t.add_row(
        "live",
        f"present {len(live.get('present_nodes') or [])}  "
        f"open {len(live.get('open_nodes') or [])}",
    )
    t.add_row(
        "committed",
        f"present {len(committed.get('present_nodes') or [])}  "
        f"open {len(committed.get('open_nodes') or [])}",
    )
    t.add_row("targets", f"{covered}/{len(targets)} covered")
    shallow = state.get("_coarse_shallow_closed")
    if isinstance(shallow, int) and shallow >= 0 and coarse:
        t.add_row("coarse DAG", f"shallow-closed {shallow}/{len(coarse)}")
    else:
        t.add_row("coarse DAG", str(len(coarse)))
    t.add_row("protected reapproval", str(len(protected)))
    return Panel(t, title="Nodes & coverage", border_style="green")


def _panel_contract(state: Dict[str, Any], metadata: Dict[str, Any]) -> Panel:
    live_records = state.get("local_closure_records") or {}
    com_records = state.get("committed_local_closure_records") or {}
    live_v = len(live_records)
    com_v = len(com_records)
    live_unverified = len(state.get("local_closure_unverified_nodes") or [])
    com_unverified = len(state.get("committed_local_closure_unverified_nodes") or [])
    live_failures = len(state.get("local_closure_failures") or {})
    com_failures = len(state.get("committed_local_closure_failures") or {})

    validation = state.get("validation_summary") or {}
    val_errs = validation.get("validation_errors") or []
    open_rejections = state.get("open_rejections") or []

    expected = {"cycle", "phase", "live", "committed", "coverage"}
    present_fields = {k for k in expected if k in state}
    schema_ok = expected.issubset(present_fields)

    t = Table.grid(padding=(0, 1))
    t.add_column(style="dim", justify="right")
    t.add_column()
    t.add_row(
        "live closure",
        f"verified {live_v}  unverified {live_unverified}  failed {live_failures}",
    )
    t.add_row(
        "committed closure",
        f"verified {com_v}  unverified {com_unverified}  failed {com_failures}",
    )
    t.add_row("validation errors", str(len(val_errs)))
    t.add_row("open rejections", str(len(open_rejections)))
    t.add_row(
        "schema fields",
        ("[green]ok[/green]" if schema_ok else f"[red]missing: {sorted(expected - present_fields)}[/red]"),
    )
    native_kinds = metadata.get("native_history_kinds") or []
    if native_kinds:
        t.add_row("history kinds", ", ".join(str(k) for k in native_kinds))
    return Panel(t, title="Kernel contract", border_style="yellow")


# ---------- chat tail ------------------------------------------------------


@dataclass
class ChatPicker:
    cycle: int = 0
    artifact_index: int = 0  # 0 = newest; cycle through with 'n'


def _list_live_artifact_dirs(repo: Path) -> List[Path]:
    root = repo / ".trellis" / "chats" / "live"
    if not root.is_dir():
        return []
    dirs = [c for c in root.iterdir() if c.is_dir()]
    dirs.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return dirs


def _panel_chat(repo: Path, cycle: int, picker: ChatPicker, max_lines: int) -> Panel:
    dirs = _list_live_artifact_dirs(repo)
    if not dirs:
        return Panel(Text("(no live chat artifacts)", style="dim"), title="Live chat", border_style="blue")

    idx = picker.artifact_index % len(dirs)
    artifact_dir = dirs[idx]

    files = {
        "prompt": "",  # skip prompt in tail
        "output": _read_text(artifact_dir / "output.log"),
        "transcriptJsonl": _read_text(artifact_dir / "transcript.jsonl"),
        "transcriptJson": _read_text(artifact_dir / "transcript.json"),
    }
    data = chat_history._build_artifact_chat_data(artifact_dir.name, files)
    entries: List[Dict[str, Any]] = data.get("entries") or []
    entries = [e for e in entries if e.get("role") != "prompt"]

    lines: List[Text] = []
    for entry in entries[-max_lines:]:
        role = str(entry.get("role") or "?")
        text = str(entry.get("text") or "")
        role_color = {
            "assistant": "green",
            "user": "cyan",
            "system": "yellow",
            "tool": "magenta",
        }.get(role, "white")
        first_line = text.strip().splitlines()[0] if text.strip() else ""
        line = Text()
        line.append(f"[{role}] ", style=f"bold {role_color}")
        line.append(first_line[:160])
        lines.append(line)
        for extra in text.strip().splitlines()[1:4]:
            sub = Text()
            sub.append("    ")
            sub.append(extra[:160], style="dim")
            lines.append(sub)

    title = f"Live chat: {artifact_dir.name}  ({idx + 1}/{len(dirs)})"
    body = Group(*lines) if lines else Text("(no entries yet)", style="dim")
    return Panel(body, title=title, border_style="blue")


def _read_text(path: Path) -> str:
    try:
        with path.open("rb") as f:
            return f.read().decode("utf-8", errors="replace")
    except OSError:
        return ""


# ---------- layout ---------------------------------------------------------


def _build_layout(
    repo: Path,
    view: Dict[str, Any],
    metadata: Dict[str, Any],
    picker: ChatPicker,
    chat_lines: int,
    show_stats: bool,
    show_chat: bool,
    paused: bool,
) -> Layout:
    state = view.get("state", {})
    runtime_info = viewer_adapter._load_runtime_info(repo)

    root = Layout()
    children: List[Layout] = []

    if show_stats:
        top = Layout(name="top", size=6)
        top.split_row(
            Layout(_panel_run(repo, runtime_info, metadata)),
            Layout(_panel_cycle(state)),
        )
        mid = Layout(name="mid", size=11)
        mid.split_row(
            Layout(_panel_pipeline(view)),
            Layout(_panel_nodes(state)),
            Layout(_panel_contract(state, metadata)),
        )
        children.append(top)
        children.append(mid)

    if show_chat:
        children.append(Layout(_panel_chat(repo, _safe_int(state.get("cycle")), picker, chat_lines), name="chat"))

    footer_txt = Text()
    footer_txt.append("  [q]", style="bold")
    footer_txt.append(" quit   ")
    footer_txt.append("[p]", style="bold")
    footer_txt.append(f" {'resume' if paused else 'pause'}   ")
    footer_txt.append("[n]", style="bold")
    footer_txt.append(" next chat   ")
    footer_txt.append("[s]", style="bold")
    footer_txt.append(" toggle stats   ")
    footer_txt.append("[c]", style="bold")
    footer_txt.append(" toggle chat")
    if paused:
        footer_txt.append("   [PAUSED]", style="bold red")
    children.append(Layout(Panel(footer_txt, border_style="dim"), name="footer", size=3))

    root.split_column(*children)
    return root


# ---------- input ----------------------------------------------------------


class _RawInput:
    """Non-blocking single-char stdin reader; no-op if stdin is not a tty."""

    def __init__(self) -> None:
        self.enabled = sys.stdin.isatty()
        self._old: Any = None

    def __enter__(self) -> "_RawInput":
        if self.enabled:
            try:
                self._old = termios.tcgetattr(sys.stdin.fileno())
                tty.setcbreak(sys.stdin.fileno())
            except (termios.error, OSError):
                self.enabled = False
        return self

    def __exit__(self, *_: Any) -> None:
        if self.enabled and self._old is not None:
            try:
                termios.tcsetattr(sys.stdin.fileno(), termios.TCSADRAIN, self._old)
            except (termios.error, OSError):
                pass

    def read_char(self) -> Optional[str]:
        if not self.enabled:
            return None
        r, _, _ = select.select([sys.stdin], [], [], 0)
        if not r:
            return None
        try:
            return sys.stdin.read(1)
        except OSError:
            return None


# ---------- main loop ------------------------------------------------------


def _snapshot(repo: Path) -> Tuple[Dict[str, Any], Dict[str, Any]]:
    try:
        view = viewer_adapter._build_live_viewer_state(repo)
    except Exception as exc:  # noqa: BLE001
        view = {"state": {}, "meta": {"error": str(exc)}}
    metadata = _read_runtime_metadata(repo)
    state = view.get("state", {})
    hint = _closure_version(state.get("committed_local_closure_records") or {}) or _closure_version(
        state.get("local_closure_records") or {}
    )
    if hint:
        metadata.setdefault("_closure_version_hint", hint)

    raw = _read_protocol_state(repo)
    deps = raw.get("committed_deps") or {}
    coarse = state.get("coarse_dag_nodes") or []
    committed = state.get("committed", {}) or {}
    present = committed.get("present_nodes") or []
    open_nodes = committed.get("open_nodes") or []
    if coarse and deps:
        try:
            state["_coarse_shallow_closed"] = _coarse_shallow_closed(coarse, present, open_nodes, deps)
        except RecursionError:
            state["_coarse_shallow_closed"] = -1
    return view, metadata


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="trellis-monitor",
        description="Terminal monitor for an active trellis run.",
    )
    parser.add_argument("--repo", type=Path, default=Path.cwd(), help="Repo path (default: cwd; walks up for .trellis/)")
    parser.add_argument("--refresh", type=float, default=1.0, help="Stats refresh interval seconds (default 1.0)")
    parser.add_argument("--chat-lines", type=int, default=24, help="Max chat lines to display")
    parser.add_argument("--once", action="store_true", help="Print one snapshot and exit")
    parser.add_argument("--no-chat", action="store_true", help="Hide chat panel")
    parser.add_argument("--chat-only", action="store_true", help="Hide stats panels")
    args = parser.parse_args(argv)

    repo = _find_repo(args.repo)
    if not (repo / ".trellis").is_dir():
        print(f"error: no .trellis/ found from {args.repo}", file=sys.stderr)
        return 2

    show_stats = not args.chat_only
    show_chat = not args.no_chat
    picker = ChatPicker()
    paused = False

    console = Console()

    if args.once:
        view, metadata = _snapshot(repo)
        layout = _build_layout(repo, view, metadata, picker, args.chat_lines, show_stats, show_chat, paused=False)
        console.print(layout)
        return 0

    view, metadata = _snapshot(repo)
    layout = _build_layout(repo, view, metadata, picker, args.chat_lines, show_stats, show_chat, paused)
    last_refresh = time.time()

    with _RawInput() as keys, Live(layout, console=console, refresh_per_second=8, screen=True) as live:
        while True:
            ch = keys.read_char()
            if ch:
                if ch in ("q", "Q", "\x03"):
                    break
                if ch in ("p", "P"):
                    paused = not paused
                elif ch in ("n", "N"):
                    picker.artifact_index += 1
                elif ch in ("s", "S"):
                    show_stats = not show_stats or not show_chat  # keep at least one
                    if not show_stats and not show_chat:
                        show_stats = True
                elif ch in ("c", "C"):
                    show_chat = not show_chat
                    if not show_stats and not show_chat:
                        show_chat = True

            if not paused and time.time() - last_refresh >= args.refresh:
                view, metadata = _snapshot(repo)
                last_refresh = time.time()

            layout = _build_layout(repo, view, metadata, picker, args.chat_lines, show_stats, show_chat, paused)
            live.update(layout)
            time.sleep(0.1)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
