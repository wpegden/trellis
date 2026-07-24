"""Query Gemini usage for account-rotation decisions.

Thin wrapper around `trellis.gemini_quota_api.fetch_gemini_user_quota`,
which calls the Code Assist `retrieveUserQuota` endpoint directly.

The return shape is the one `gemini_accounts` consumes: each value
carries `remaining_pct` (a 0-100 float). Categories are keyed by display
name ("Pro", "Flash", "Flash Lite") rather than raw modelId.
"""

from __future__ import annotations

from pathlib import Path
from typing import Dict, Optional

from trellis.gemini_quota_api import fetch_gemini_user_quota


def check_gemini_usage(
    *,
    burst_home: Optional[Path] = None,
    timeout: float = 10.0,
) -> Dict[str, dict]:
    """Return per-category Gemini quota via the Code Assist HTTPS API.

    Empty dict on any error — never raises. The 3-strike circuit breaker
    in `gemini_quota_api` will quietly suspend further attempts after
    sustained failure.
    """
    return fetch_gemini_user_quota(
        burst_home=burst_home,
        timeout=timeout,
    )
