"""Usage / cost rollup for a trellis runtime.

Reads the per-burst cost-ledger.jsonl written by `append_cost_ledger` in
`trellis.agents.tmux_backend` and prints rollups. Invoked via
`python3 -m trellis.usage_report <runtime_root>` or
`scripts/trellis.sh report <runtime_root>`.

Rollup axes:
  - per provider (claude, gemini, codex)
  - per (provider, role) — e.g. claude/worker vs claude/reviewer
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Optional


def _read_runtime_metadata(runtime_root: Path) -> dict:
    path = runtime_root / "runtime_metadata.json"
    if not path.is_file():
        raise SystemExit(f"no runtime_metadata.json at {path}")
    return json.loads(path.read_text(encoding="utf-8"))


def _ledger_path_for_runtime(runtime_root: Path) -> Path:
    meta = _read_runtime_metadata(runtime_root)
    repo = meta.get("repo_path")
    if not repo:
        raise SystemExit("runtime_metadata has no repo_path")
    return Path(repo) / ".trellis" / "logs" / "cost-ledger.jsonl"


def _normalize_usage_for_rollup(provider: str, u: dict) -> dict:
    """Apply provider-key aliasing. Mirrors summarize_cost_ledger logic."""
    if not isinstance(u, dict):
        return {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0}
    raw_input = int(u.get("input_tokens", 0) or u.get("input", 0) or 0)
    cache_read = int(
        u.get("cache_read_input_tokens", 0)
        or u.get("cached_input_tokens", 0)
        or u.get("cached", 0)
        or 0
    )
    if provider == "codex":
        new_input = max(0, raw_input - cache_read)
    else:
        new_input = raw_input
    out = int(u.get("output_tokens", 0) or u.get("output", 0) or 0)
    cw = int(u.get("cache_creation_input_tokens", 0) or 0)
    return {"input": new_input, "output": out, "cache_read": cache_read, "cache_write": cw}


def _delegacy_codex_rows(rows: list) -> list:
    """Convert legacy-cumulative codex rows into per-burst-delta rows.

    Codex sessions resume across many bursts; each `turn.completed` event
    reports usage cumulative-from-session-start. Until the 2026-06-04 fix
    in `trellis.agents.codex_headless`, both `cost_usd` and `usage` on
    each ledger row were the running session total (and
    `session_total_cost_usd` was None). Summing those columns across rows
    over-counts the carried context by up to ~10x on long resumed
    sessions.

    This shim walks `rows` in ledger order, and for each codex row whose
    `session_id` has appeared before AND whose `session_total_cost_usd`
    is None (legacy marker) AND whose `usage.input_tokens` is greater
    than the prior row's, subtracts the prior cumulative to recover a
    per-burst delta. Non-codex rows and rows already carrying explicit
    per-burst values (post-fix: `session_total_cost_usd` is populated)
    pass through unchanged.

    Returns a new list of row dicts (originals are not mutated).
    """
    out = []
    prior_by_sid: dict = {}  # session_id → (cum_cost, cum_usage_dict)
    cum_keys = (
        "input_tokens", "output_tokens",
        "cached_input_tokens", "reasoning_output_tokens",
    )
    for r in rows:
        if (r.get("provider") or "") != "codex":
            out.append(r)
            continue
        sid = r.get("session_id")
        if not sid:
            out.append(r)
            continue
        # Post-fix rows have an explicit cumulative snapshot — trust them.
        if r.get("session_total_cost_usd") is not None:
            # Still record the running cumulative so a mixed-era ledger
            # (legacy rows followed by post-fix rows) bookkeeps correctly.
            cum_u = r.get("session_total_usage") or {}
            prior_by_sid[sid] = (
                float(r.get("session_total_cost_usd") or 0.0),
                {k: int(cum_u.get(k, 0) or 0) for k in cum_keys},
            )
            out.append(r)
            continue
        # Legacy-cumulative codex row. Compute delta from prior.
        cur_cost = r.get("cost_usd")
        cur_u = r.get("usage") or {}
        try:
            cur_cost_f = float(cur_cost) if isinstance(cur_cost, (int, float)) else 0.0
        except Exception:
            cur_cost_f = 0.0
        prev_cost, prev_u = prior_by_sid.get(sid, (0.0, {}))
        cur_input = int(cur_u.get("input_tokens", 0) or 0)
        prev_input = int(prev_u.get("input_tokens", 0) or 0)
        # Only treat as cumulative when input_tokens monotonically grew.
        # Otherwise (e.g. a 0-token failed-turn row, or a session-id collision
        # across distinct sessions), pass through unchanged.
        if cur_input >= prev_input and (prev_cost > 0 or prev_input > 0):
            delta_cost = max(0.0, cur_cost_f - prev_cost)
            delta_u = dict(cur_u)
            for k in cum_keys:
                delta_u[k] = max(0, int(cur_u.get(k, 0) or 0) - int(prev_u.get(k, 0) or 0))
            new_row = dict(r)
            new_row["cost_usd"] = delta_cost
            new_row["usage"] = delta_u
            new_row["_legacy_cumulative_codex"] = True  # debug marker
            out.append(new_row)
        else:
            out.append(r)
        prior_by_sid[sid] = (
            max(prev_cost, cur_cost_f),
            {k: max(int(prev_u.get(k, 0) or 0), int(cur_u.get(k, 0) or 0)) for k in cum_keys},
        )
    return out


def _roll_by(path: Path, key_fn):
    rows = []
    if not path.is_file():
        return rows, 0
    n_total = 0
    with path.open("r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                r = json.loads(line)
            except Exception:
                continue
            n_total += 1
            rows.append(r)
    # Recover per-burst deltas for legacy-cumulative codex rows before
    # aggregation. Post-fix rows (with `session_total_cost_usd` set) pass
    # through unchanged.
    rows = _delegacy_codex_rows(rows)
    agg: dict = {}
    for r in rows:
        key = key_fn(r)
        a = agg.setdefault(
            key,
            {"bursts": 0, "ok": 0, "duration_s": 0.0, "cost_usd": 0.0,
             "input": 0, "output": 0, "cache_read": 0, "cache_write": 0,
             "messages": 0, "bursts_with_msgs": 0},
        )
        a["bursts"] += 1
        if r.get("ok"):
            a["ok"] += 1
        a["duration_s"] += float(r.get("duration_seconds") or 0)
        c = r.get("cost_usd")
        if isinstance(c, (int, float)):
            a["cost_usd"] += c
        mc = r.get("message_count")
        if isinstance(mc, int) and mc >= 0:
            a["messages"] += mc
            a["bursts_with_msgs"] += 1
        norm = _normalize_usage_for_rollup(r.get("provider", ""), r.get("usage") or {})
        for k in ("input", "output", "cache_read", "cache_write"):
            a[k] += norm[k]
    return [(k, v) for k, v in sorted(agg.items())], n_total


def _fmt_hours(secs: float) -> str:
    return f"{secs / 3600:.2f}"


def _fmt_tokens(n: int) -> str:
    if n >= 1_000_000:
        return f"{n / 1_000_000:.2f}M"
    if n >= 1_000:
        return f"{n / 1_000:.1f}K"
    return f"{n}"


def _fmt_usd(x: float) -> str:
    if x == 0:
        return "—"
    return f"${x:,.2f}"


def _print_table(title: str, col_labels, rows):
    print(title)
    widths = [max(len(c), *(len(str(r[i])) for r in rows)) if rows else len(c) for i, c in enumerate(col_labels)]
    row_fmt = "  ".join(f"{{:>{w}}}" for w in widths)
    print("  " + row_fmt.format(*col_labels))
    print("  " + row_fmt.format(*["-" * w for w in widths]))
    for r in rows:
        print("  " + row_fmt.format(*[str(x) for x in r]))
    print()


def _check_ledger_path_for_runtime(runtime_root: Path) -> Path:
    meta = _read_runtime_metadata(runtime_root)
    repo = meta.get("repo_path")
    if not repo:
        raise SystemExit("runtime_metadata has no repo_path")
    return Path(repo) / ".trellis" / "logs" / "check-ledger.jsonl"


def _roll_check_ledger(path: Path, *, kind_filter: Optional[str] = None):
    """Per-subcommand totals from the deterministic check-ledger, optionally
    filtered to a specific `kind` (e.g. 'check' for check.py subcommands or
    'git' for direct git subprocesses). Rows with no `kind` are treated as
    `kind == 'check'` for backward compatibility.
    """
    if not path.is_file():
        return [], 0
    agg: dict = {}
    n_total = 0
    with path.open("r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                r = json.loads(line)
            except Exception:
                continue
            kind = r.get("kind") or "check"
            if kind_filter is not None and kind != kind_filter:
                continue
            n_total += 1
            sub = r.get("subcommand") or "?"
            a = agg.setdefault(sub, {"count": 0, "ok": 0, "duration_s": 0.0})
            a["count"] += 1
            if r.get("ok"):
                a["ok"] += 1
            a["duration_s"] += float(r.get("duration_seconds") or 0)
    return [(k, v) for k, v in sorted(agg.items(), key=lambda kv: -kv[1]["duration_s"])], n_total


def _roll_event_log_by_stage(path: Path):
    """Sum wall-clock seconds the supervisor spent in each stage.

    Uses `ts_ms` deltas between consecutive events: the time from event N's
    ts_ms to event N+1's ts_ms is attributed to event N's `stage`. Records
    without ts_ms (ts_ms == 0, i.e., written before the field was added) are
    skipped.
    """
    if not path.is_file():
        return [], 0
    rows = []
    with path.open("r", encoding="utf-8") as f:
        for line in f:
            try:
                r = json.loads(line)
            except Exception:
                continue
            rows.append(r)
    agg: dict = {}
    counted = 0
    for i in range(len(rows) - 1):
        a, b = rows[i], rows[i + 1]
        ta, tb = int(a.get("ts_ms") or 0), int(b.get("ts_ms") or 0)
        if ta <= 0 or tb <= ta:
            continue
        stage = str(a.get("stage") or "?")
        dt = (tb - ta) / 1000.0
        bucket = agg.setdefault(stage, {"intervals": 0, "duration_s": 0.0})
        bucket["intervals"] += 1
        bucket["duration_s"] += dt
        counted += 1
    return [(k, v) for k, v in sorted(agg.items(), key=lambda kv: -kv[1]["duration_s"])], counted


def report(runtime_root: Path, ledger_path: Optional[Path] = None) -> None:
    ledger = ledger_path or _ledger_path_for_runtime(runtime_root)
    if not ledger.is_file():
        print(f"no cost-ledger at {ledger}", file=sys.stderr)
        return

    # Provider rollup
    by_prov, n = _roll_by(ledger, lambda r: (r.get("provider") or "?",))
    cols = ["provider", "bursts", "ok", "msgs", "hours", "input", "output", "cache_r", "cache_w", "USD"]
    table = []
    gt_bursts = gt_ok = 0
    gt_msgs = 0
    gt_secs = gt_cost = 0.0
    gt_in = gt_out = gt_cr = gt_cw = 0
    for (prov,), a in by_prov:
        msg_cell = str(a["messages"]) if a["bursts_with_msgs"] else "—"
        table.append([
            prov, a["bursts"], a["ok"], msg_cell, _fmt_hours(a["duration_s"]),
            _fmt_tokens(a["input"]), _fmt_tokens(a["output"]),
            _fmt_tokens(a["cache_read"]), _fmt_tokens(a["cache_write"]),
            _fmt_usd(a["cost_usd"]),
        ])
        gt_bursts += a["bursts"]; gt_ok += a["ok"]
        gt_msgs += a["messages"]
        gt_secs += a["duration_s"]; gt_cost += a["cost_usd"]
        gt_in += a["input"]; gt_out += a["output"]
        gt_cr += a["cache_read"]; gt_cw += a["cache_write"]
    if table:
        table.append([
            "TOTAL", gt_bursts, gt_ok, str(gt_msgs) if gt_msgs else "—",
            _fmt_hours(gt_secs),
            _fmt_tokens(gt_in), _fmt_tokens(gt_out),
            _fmt_tokens(gt_cr), _fmt_tokens(gt_cw),
            _fmt_usd(gt_cost),
        ])
    _print_table(f"Runtime: {runtime_root}\nLedger:  {ledger}  ({n} bursts)\n\nPer provider:", cols, table)

    # (provider, role) rollup
    by_pr, _ = _roll_by(ledger, lambda r: (r.get("provider") or "?", r.get("role") or "?"))
    cols2 = ["provider", "role", "bursts", "ok", "msgs", "hours", "input", "output", "cache_r", "cache_w", "USD"]
    rows2 = []
    for (prov, role), a in by_pr:
        msg_cell = str(a["messages"]) if a["bursts_with_msgs"] else "—"
        rows2.append([
            prov, role, a["bursts"], a["ok"], msg_cell, _fmt_hours(a["duration_s"]),
            _fmt_tokens(a["input"]), _fmt_tokens(a["output"]),
            _fmt_tokens(a["cache_read"]), _fmt_tokens(a["cache_write"]),
            _fmt_usd(a["cost_usd"]),
        ])
    _print_table("Per (provider, role):", cols2, rows2)

    # Deterministic check rollup (check.py subcommands) — kind='check'.
    # Rows with no explicit kind default to 'check' for backward compat.
    check_ledger = _check_ledger_path_for_runtime(runtime_root)
    if check_ledger.is_file():
        by_sub, n_checks = _roll_check_ledger(check_ledger, kind_filter="check")
        cols3 = ["subcommand", "calls", "ok", "total_s", "mean_s"]
        rows3 = []
        gt_calls = gt_ok = 0
        gt_secs = 0.0
        for sub, a in by_sub:
            mean = a["duration_s"] / a["count"] if a["count"] else 0
            rows3.append([sub, a["count"], a["ok"], f"{a['duration_s']:.2f}", f"{mean:.3f}"])
            gt_calls += a["count"]; gt_ok += a["ok"]; gt_secs += a["duration_s"]
        if rows3:
            rows3.append(["TOTAL", gt_calls, gt_ok, f"{gt_secs:.2f}", ""])
        _print_table(
            f"Deterministic checks  ({n_checks} check.py calls, "
            f"{check_ledger}):",
            cols3,
            rows3,
        )
        # Git subprocess rollup — kind='git'. Only shown if there's data,
        # to avoid cluttering pre-feature reports.
        by_git, n_git = _roll_check_ledger(check_ledger, kind_filter="git")
        if by_git:
            cols_g = ["subcommand", "calls", "ok", "total_s", "mean_s"]
            rows_g = []
            gg_calls = gg_ok = 0
            gg_secs = 0.0
            for sub, a in by_git:
                mean = a["duration_s"] / a["count"] if a["count"] else 0
                rows_g.append([sub, a["count"], a["ok"], f"{a['duration_s']:.2f}", f"{mean:.3f}"])
                gg_calls += a["count"]; gg_ok += a["ok"]; gg_secs += a["duration_s"]
            rows_g.append(["TOTAL", gg_calls, gg_ok, f"{gg_secs:.2f}", ""])
            _print_table(
                f"Git subprocesses  ({n_git} git calls from the kernel):",
                cols_g,
                rows_g,
            )
    else:
        print(f"Deterministic checks: no check-ledger yet at {check_ledger}\n")

    # Per-stage wall-clock
    event_log = runtime_root / "event_log.jsonl"
    if event_log.is_file():
        by_stage, n_intervals = _roll_event_log_by_stage(event_log)
        cols4 = ["stage", "intervals", "total_s", "hours"]
        rows4 = [
            [stage, a["intervals"], f"{a['duration_s']:.1f}", _fmt_hours(a["duration_s"])]
            for stage, a in by_stage
        ]
        if rows4:
            total_stage_s = sum(a["duration_s"] for _, a in by_stage)
            rows4.append(["TOTAL", n_intervals, f"{total_stage_s:.1f}", _fmt_hours(total_stage_s)])
        _print_table(
            f"Per-stage wall-clock  ({n_intervals} timed intervals from "
            f"event_log.jsonl ts_ms):",
            cols4,
            rows4,
        )
        if not rows4:
            print(
                "  (no ts_ms-bearing records yet — ts_ms was added recently; "
                "old event logs record as 0 and are skipped)\n"
            )

    # Current quota windows — read latest snapshot per provider from the
    # quota-snapshots ledger. Best-effort: missing/empty ledger just means
    # no probes have fired yet (or all suspended by circuit breaker).
    _print_quota_windows(_quota_snapshots_path_for_runtime(runtime_root))

    note = (
        "Notes: Gemini USD is $0 because it's a plan-based subscription; we "
        "don't compute per-token costs. Claude/codex USD is the per-burst API-"
        "equivalent price at published rates — if you're on a subscription, "
        "your actual bill is the flat monthly fee, not this figure."
    )
    print(note)


def _quota_snapshots_path_for_runtime(runtime_root: Path) -> Path:
    meta = _read_runtime_metadata(runtime_root)
    repo = meta.get("repo_path")
    if not repo:
        raise SystemExit("runtime_metadata has no repo_path")
    return Path(repo) / ".trellis" / "logs" / "quota-snapshots.jsonl"


def _fmt_resets_in(seconds: Optional[int]) -> str:
    if seconds is None:
        return "—"
    s = max(0, int(seconds))
    if s >= 86400:
        d, rem = divmod(s, 86400)
        h, _ = divmod(rem, 3600)
        return f"{d}d {h}h"
    if s >= 3600:
        h, rem = divmod(s, 3600)
        m, _ = divmod(rem, 60)
        return f"{h}h {m}m"
    if s >= 60:
        m, _ = divmod(s, 60)
        return f"{m}m"
    return f"{s}s"


def _fmt_pct(p: Optional[float]) -> str:
    if p is None:
        return "—"
    if p == 0:
        return "0%"
    if p < 0.1:
        return f"{p:.2f}%"
    if p < 1:
        return f"{p:.1f}%"
    return f"{p:.0f}%"


def _print_quota_windows(path: Path) -> None:
    """Read the quota-snapshots ledger, find the latest probe per provider,
    and render a "Current quota windows" section. Each row shows:
      - provider / window / pct_used / resets_in / monthly_burn_pct
    `monthly_burn_pct` = pct_used × (window_seconds / 30-day month) — the
    cross-provider "what fraction of a monthly subscription is this run
    actually consuming" metric.
    """
    if not path.is_file():
        return
    latest: Dict[str, dict] = {}
    suspended: Dict[str, str] = {}
    last_error: Dict[str, str] = {}
    with path.open("r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                r = json.loads(line)
            except Exception:
                continue
            prov = r.get("provider")
            if not prov:
                continue
            if r.get("ok"):
                latest[prov] = r
            else:
                last_error[prov] = str(r.get("error") or "unknown")
            if r.get("suspended"):
                suspended[prov] = str(r.get("suspended_reason") or "")
    if not latest and not last_error and not suspended:
        return
    cols = ["provider", "scope", "pct_used", "resets_in", "monthly_burn"]
    rows: list = []
    now_ts = int(__import__("time").time())
    for prov in sorted(set(list(latest.keys()) + list(last_error.keys()) + list(suspended.keys()))):
        snap = latest.get(prov)
        if snap is None:
            err = last_error.get(prov, "no successful probe yet")
            rows.append([prov, "(probe failed)", "—", "—", err[:60]])
            continue
        windows = snap.get("windows") or []
        models = snap.get("models") or []
        for w in windows:
            resets_in = w.get("resets_in_seconds")
            if resets_in is None and isinstance(w.get("resets_at"), (int, float)):
                resets_in = max(0, int(w["resets_at"]) - now_ts)
            pct_used = w.get("pct_used")
            band = " (band)" if w.get("pct_used_kind") == "band" else ""
            scope = f"{w.get('name', '?')}{band}"
            rows.append([
                prov, scope, _fmt_pct(pct_used),
                _fmt_resets_in(resets_in),
                _fmt_pct(w.get("monthly_burn_pct")),
            ])
        for m in models:
            resets_in = m.get("resets_in_seconds")
            scope = f"model:{m.get('category', m.get('model', '?'))}"
            rows.append([
                prov, scope, _fmt_pct(m.get("pct_used")),
                _fmt_resets_in(resets_in),
                _fmt_pct(m.get("monthly_burn_pct")),
            ])
    if suspended:
        for prov, reason in sorted(suspended.items()):
            rows.append([prov, "(suspended)", "—", "—", reason[:60]])
    if rows:
        _print_table(
            f"Current quota windows  (latest probe per provider, from "
            f"{path}):\n  monthly_burn = pct_used × window_fraction_of_30-day_month",
            cols, rows,
        )


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="Report usage + cost for a trellis runtime.")
    ap.add_argument("runtime_root", type=Path, help="Path to a runtime root directory")
    ap.add_argument("--ledger", type=Path, default=None, help="Explicit cost-ledger.jsonl path (otherwise derived)")
    args = ap.parse_args(argv)
    report(args.runtime_root.resolve(), args.ledger.resolve() if args.ledger else None)
    return 0


if __name__ == "__main__":
    sys.exit(main())
