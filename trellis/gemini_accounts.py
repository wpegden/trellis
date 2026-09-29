"""Gemini account rotation using burst-home auth snapshots."""

from __future__ import annotations

import json
import os
import time
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from trellis.gemini_usage import check_gemini_usage

LOW_BUDGET_THRESHOLD = int(os.environ.get("TRELLIS_GEMINI_LOW_BUDGET_THRESHOLD", "10"))
BUDGET_CHECK_COOLDOWN_SECONDS = float(
    os.environ.get("TRELLIS_GEMINI_BUDGET_CHECK_COOLDOWN_SECONDS", "300")
)
DEFAULT_PRIMARY_ACCOUNT = os.environ.get(
    "TRELLIS_GEMINI_PRIMARY_ACCOUNT",
    "primary@example.com",
)

_last_budget_check_by_home: Dict[Path, float] = {}


def _gemini_home(*, burst_home: Optional[Path]) -> Path:
    base = burst_home or Path.home()
    return base.resolve() / ".gemini"


def available_accounts(
    *,
    burst_home: Optional[Path] = None,
) -> List[str]:
    # Post-bwrap-only: gemini home is supervisor-owned (per-burst fake-home
    # hard-links the supervisor's ~/.gemini), so direct iteration works.
    accounts_dir = _gemini_home(burst_home=burst_home) / "accounts"
    try:
        if not accounts_dir.exists():
            return []
        return sorted(p.name for p in accounts_dir.iterdir() if p.is_dir())
    except OSError:
        return []


def rotation_available(
    *,
    burst_home: Optional[Path] = None,
) -> bool:
    return len(available_accounts(burst_home=burst_home)) >= 2


def gemini_api_env_keys_to_forward(
    *,
    burst_home: Optional[Path] = None,
) -> Tuple[str, ...]:
    if rotation_available(burst_home=burst_home):
        return ()
    return ("GEMINI_API_KEY", "GOOGLE_API_KEY")


def active_account(
    *,
    burst_home: Optional[Path] = None,
) -> Optional[str]:
    # Post-bwrap-only: read the supervisor-side gemini home directly.
    ga_path = _gemini_home(burst_home=burst_home) / "google_accounts.json"
    try:
        return json.loads(ga_path.read_text(encoding="utf-8")).get("active", "") or None
    except Exception:
        return None


def switch_account(
    email: str,
    *,
    burst_home: Optional[Path] = None,
) -> bool:
    gemini_home = _gemini_home(burst_home=burst_home)
    account_dir = gemini_home / "accounts" / email
    # Post-bwrap-only: write to the supervisor-side gemini home directly.
    for filename in ("oauth_creds.json", "google_accounts.json"):
        src = account_dir / filename
        dst = gemini_home / filename
        try:
            dst.write_bytes(src.read_bytes())
        except Exception:
            return False
    print(f"  Switched Gemini account to {email}")
    return True


def check_budget_low(
    *,
    burst_home: Optional[Path] = None,
) -> Tuple[bool, Dict[str, dict]]:
    if burst_home is None:
        return False, {}
    try:
        stats = check_gemini_usage(
            burst_home=burst_home,
        )
        if not stats:
            return False, {}
        for model_data in stats.values():
            if model_data.get("remaining_pct", 100) < LOW_BUDGET_THRESHOLD:
                return True, stats
        return False, stats
    except Exception:
        return False, {}


def ensure_budget(
    *,
    burst_home: Optional[Path] = None,
    primary_account: Optional[str] = None,
) -> Optional[str]:
    if not rotation_available(burst_home=burst_home):
        return active_account(burst_home=burst_home)

    current = active_account(burst_home=burst_home)
    primary = primary_account or DEFAULT_PRIMARY_ACCOUNT or current
    accounts = available_accounts(burst_home=burst_home)

    is_low, stats = check_budget_low(
        burst_home=burst_home,
    )
    if not stats:
        print("  WARNING: Gemini budget check unavailable; falling back to default account")
        if primary in accounts and current != primary:
            if switch_account(primary, burst_home=burst_home):
                return primary
        return current or primary

    if not is_low:
        if primary and current and current != primary:
            if primary in accounts and switch_account(
                primary, burst_home=burst_home
            ):
                primary_low, _ = check_budget_low(
                    burst_home=burst_home,
                )
                if primary_low:
                    switch_account(current, burst_home=burst_home)
                    return current
                return primary
        return current or primary

    low_models = [
        model for model, data in stats.items() if data.get("remaining_pct", 100) < LOW_BUDGET_THRESHOLD
    ]
    print(f"  Gemini budget low on {current}: {low_models}")

    for email in accounts:
        if email == current:
            continue
        if not switch_account(email, burst_home=burst_home):
            continue
        alt_low, alt_stats = check_budget_low(
            burst_home=burst_home,
        )
        if not alt_low:
            print(f"  Rotated to {email} (budget OK)")
            return email
        low_alt = [
            model
            for model, data in alt_stats.items()
            if data.get("remaining_pct", 100) < LOW_BUDGET_THRESHOLD
        ]
        print(f"  {email} also low: {low_alt}")

    print("  WARNING: All Gemini accounts are low on budget")
    if current:
        switch_account(current, burst_home=burst_home)
    return current or primary


def maybe_ensure_budget(
    *,
    burst_home: Optional[Path] = None,
    primary_account: Optional[str] = None,
) -> Optional[str]:
    if not rotation_available(burst_home=burst_home):
        return active_account(burst_home=burst_home)
    home = _gemini_home(burst_home=burst_home)
    now = time.monotonic()
    last = _last_budget_check_by_home.get(home)
    if last is not None and now - last < BUDGET_CHECK_COOLDOWN_SECONDS:
        return active_account(burst_home=burst_home)
    active = ensure_budget(
        burst_home=burst_home,
        primary_account=primary_account,
    )
    _last_budget_check_by_home[home] = time.monotonic()
    return active
