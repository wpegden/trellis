"""Append-only sidecar attempt/token ledger (E§4.4).

One JSONL row per attempt at ``<runtime>/sidecar/ledger.jsonl``.
Append is best-effort and NEVER fatal (the ``append_quota_snapshot``
posture: telemetry must never block the daemon).

RECORDER, NOT A GATE (wall-clock regime). The ledger records per-attempt
token/wall telemetry for observability, but cumulative token VOLUME never
throttles the daemon: throughput is bounded solely by the grunt pool size
(``grunts``, 2 by default) and the per-attempt wall. The daily/monthly
caps below are DISABLED by default (0 == no cap) and, when disabled,
``budget_allows_new_attempt`` is a no-op that always admits — nothing
about cumulative volume may pause, idle, suspend, or wedge the daemon. A
cap is only enforced if an operator explicitly sets a positive value.

Row shape::

    {"ts": ..., "attempt_id": "...", "node": ..., "entry_seq": N,
     "grunt": K, "status": ..., "detail": "...", "iterations": N,
     "prompt_tokens": N, "completion_tokens": N, "wall_secs": ...,
     "snapshot_sha": "...", "timings": {...}}

``timings`` (phase telemetry) appears only when the runner reported
some, and a ``cancelled`` row — written by ``_cancel_slot``, which has
no attempt result in hand — carries the identity fields, ``status``,
``detail`` and ``snapshot_sha`` alone.

No API key and no request header ever lands here, structurally:
``ModelClient.chat`` builds the Authorization header inside the call and
hands it straight to ``post``, and every transport failure surfaces as a
``ModelTransportError`` carrying the exception CLASS NAME, so no value
that saw the key is reachable from an outcome (``test_key_never_leaks``
pins the whole path — payload, error detail, attempt record, stdout).

``detail`` is a bounded diagnostic string of external origin. A
non-retryable 4xx carries the provider's error body (whitespace
collapsed, 200 chars) and a failed pre-validation carries the Lean
axioms-probe log tail (300 chars); either can quote a fragment of what
was sent, since a context-overflow 400 names the offending token count
and a compiler diagnostic quotes the candidate proof. That echo IS the
diagnostic payload — two live-run 400s were unattributable while the
body was dropped — so it is recorded as the provider wrote it.
"""

from __future__ import annotations

import json
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Optional


DAY_SECONDS = 24 * 3600.0
MONTH_SECONDS = 30 * DAY_SECONDS


def ledger_path(runtime_root: Path) -> Path:
    return Path(runtime_root) / "sidecar" / "ledger.jsonl"


def append_ledger_row(runtime_root: Path, row: Dict[str, Any]) -> None:
    """Append one row; swallow every error (never-fatal)."""
    try:
        path = ledger_path(runtime_root)
        path.parent.mkdir(parents=True, exist_ok=True)
        payload = {"ts": time.time(), **row}
        with path.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(payload, sort_keys=True) + "\n")
    except Exception:
        pass


def contains_attempt(runtime_root: Path, attempt_id: str) -> bool:
    """Has ``attempt_id`` already been bookkept?

    The exactly-once guard for STARTUP adoption (§2.4): a daemon that
    died between ``_bookkeep_outcome`` and the journal write leaves a
    row whose attempt is dead AND already accounted for; re-reaping it
    would double-count the ledger and re-record the attempt. The ledger
    is the right key because ``_bookkeep_outcome`` appends for EVERY
    real outcome (transport-class and cancels included); the one status
    that writes nothing is ``workspace_idle``, whose re-bookkeep is
    already a no-op early return.

    Startup-only: this scans the whole ledger."""
    if not attempt_id:
        return False
    try:
        lines = ledger_path(runtime_root).read_text(encoding="utf-8").splitlines()
    except OSError:
        return False
    for line in lines:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if isinstance(row, dict) and str(row.get("attempt_id", "")) == attempt_id:
            return True
    return False


@dataclass(frozen=True)
class TokenTotals:
    day_tokens: int
    month_tokens: int


def rolling_token_totals(
    runtime_root: Path, now: Optional[float] = None
) -> TokenTotals:
    """Scan the ledger for prompt+completion token totals over the
    rolling day/month windows. Malformed rows are skipped."""
    now = time.time() if now is None else now
    day = 0
    month = 0
    path = ledger_path(runtime_root)
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return TokenTotals(0, 0)
    for line in lines:
        try:
            row = json.loads(line)
            ts = float(row.get("ts", 0.0))
            tokens = int(row.get("prompt_tokens", 0)) + int(
                row.get("completion_tokens", 0)
            )
        except (ValueError, TypeError):
            continue
        age = now - ts
        if age < 0:
            continue
        if age <= MONTH_SECONDS:
            month += tokens
            if age <= DAY_SECONDS:
                day += tokens
    return TokenTotals(day_tokens=day, month_tokens=month)


def budget_allows_new_attempt(
    runtime_root: Path,
    daily_cap: int,
    monthly_cap: int,
    now: Optional[float] = None,
) -> bool:
    """Refuse a NEW attempt once a rolling window is at/over a POSITIVE
    cap. A cap of 0 (or negative) is DISABLED — that window is never
    checked. Both caps default to disabled (wall-clock regime): with the
    defaults this always returns True, so cumulative token volume never
    gates the daemon. The scan is skipped entirely when both caps are
    off, so a disabled ledger cannot even slow the poll loop."""
    if monthly_cap <= 0 and daily_cap <= 0:
        return True
    totals = rolling_token_totals(runtime_root, now=now)
    if monthly_cap > 0 and totals.month_tokens >= monthly_cap:
        return False
    if daily_cap > 0 and totals.day_tokens >= daily_cap:
        return False
    return True
