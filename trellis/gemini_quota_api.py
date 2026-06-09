"""Direct HTTPS query for Gemini per-account quota.

We hit the same Code Assist endpoint that the gemini-cli itself uses
internally:

    POST https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota
    Authorization: Bearer <oauth access token>
    Body: {"project": "<projectId>"}

Auth comes from `<burst_home>/.gemini/oauth_creds.json` — same file the
CLI maintains. If the access token is expired we refresh it against
Google's token endpoint using the OAuth client credentials baked into
the gemini-cli bundle (see `_gemini_oauth_constants.py`).

This module is **cosmetic-only-tolerant**: every error path falls
through to an empty dict. A 3-strike circuit breaker suspends further
attempts for the rest of the process after sustained failure — never
raises to the caller.

`trellis.quota_snapshots` is separate — that one scrapes a tmux pane
with `/model` (cadence-managed). Left alone.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import time
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, Optional, Tuple

from trellis._gemini_oauth_constants import (
    CODE_ASSIST_LOAD,
    CODE_ASSIST_RETRIEVE_USER_QUOTA,
    GEMINI_OAUTH_CLIENT_ID,
    GEMINI_OAUTH_CLIENT_SECRET,
    GEMINI_OAUTH_TOKEN_ENDPOINT,
)

# Window length for gemini Code Assist quotas. All categories use the
# same 24-hour rolling cap; matches the value `quota_snapshots` reports.
_GEMINI_QUOTA_WINDOW_SECONDS = 24 * 3600

# Refresh stale tokens this many seconds BEFORE they actually expire,
# to avoid burning a request only to get a 401.
_REFRESH_LEEWAY_SECONDS = 60.0

# Mirror of the circuit-breaker shape from `quota_snapshots`. Process-local;
# resets across supervisor restarts which is fine — quota tracking is
# cosmetic and the first probe per restart is genuinely useful.
_CONSECUTIVE_FAILURES: Dict[str, int] = {}
_SUSPENDED_PROVIDERS: set = set()
PROBE_FAILURE_THRESHOLD = 3
_PROVIDER_KEY = "gemini_quota_api"


# --------------------------------------------------------------------- model→category mapping

# `bucket.modelId` strings observed in the wild:
#   "gemini-2.5-pro"
#   "gemini-2.5-flash"
#   "gemini-2.5-flash-lite"
#   "gemini-2.5-flash-image"
#   ...and future "gemini-X.Y-{name}" variants.
# The `check_budget_low` caller cares only about Pro / Flash / Flash Lite
# — those are the buckets the CLI's `/model` table also exposes. Newer
# experimental categories (image, etc.) are skipped silently.
def _category_for_model_id(model_id: str) -> Optional[str]:
    s = (model_id or "").strip().lower()
    if not s:
        return None
    # Order matters: "flash-lite" must match before "flash".
    if "flash-lite" in s:
        return "Flash Lite"
    if "-pro" in s or s.endswith("-pro"):
        return "Pro"
    if "flash" in s and "image" not in s and "audio" not in s:
        return "Flash"
    return None


# --------------------------------------------------------------------- HTTP helpers


class _HTTPError(Exception):
    def __init__(self, status: int, body: str = "") -> None:
        super().__init__(f"HTTP {status}: {body[:200]}")
        self.status = status
        self.body = body


def _http_post_json(url: str, body: Dict[str, Any], headers: Dict[str, str], timeout: float = 10.0) -> Dict[str, Any]:
    data = json.dumps(body).encode("utf-8")
    h = {"Content-Type": "application/json", "Accept": "application/json", **headers}
    req = urllib.request.Request(url, data=data, headers=h, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            if not raw:
                return {}
            return json.loads(raw)
    except urllib.error.HTTPError as e:
        try:
            body_text = e.read().decode("utf-8", errors="replace")
        except Exception:
            body_text = ""
        raise _HTTPError(e.code, body_text)


def _http_post_form(url: str, fields: Dict[str, str], timeout: float = 10.0) -> Dict[str, Any]:
    data = urllib.parse.urlencode(fields).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=data,
        headers={"Content-Type": "application/x-www-form-urlencoded", "Accept": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as e:
        try:
            body_text = e.read().decode("utf-8", errors="replace")
        except Exception:
            body_text = ""
        raise _HTTPError(e.code, body_text)


# --------------------------------------------------------------------- file IO
#
# Phase 4/5 bwrap-only migration: per-burst fake-home hard-links the
# supervisor's ~/.gemini, so the supervisor user can read/write these files
# directly. No sudo wrap needed; burst_user parameter dropped.


def _read_user_file(path: Path, *, timeout: float = 5.0) -> Optional[str]:
    """Read a file. Returns None on any IO error. Never raises."""
    try:
        return path.read_text(encoding="utf-8")
    except Exception:
        return None


def _write_user_file(path: Path, content: str, *, timeout: float = 5.0) -> bool:
    """Write a file. Best-effort; returns True on success."""
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        return True
    except Exception:
        return False


def _mkdir_user(path: Path, *, timeout: float = 5.0) -> bool:
    try:
        path.mkdir(parents=True, exist_ok=True)
        return True
    except Exception:
        return False


# --------------------------------------------------------------------- token plumbing


def _now_seconds() -> float:
    return time.time()


def _now_ms() -> int:
    return int(_now_seconds() * 1000)


def _load_creds(home: Path) -> Optional[Dict[str, Any]]:
    creds_path = home / ".gemini" / "oauth_creds.json"
    raw = _read_user_file(creds_path)
    if not raw:
        return None
    try:
        return json.loads(raw)
    except Exception:
        return None


def _save_creds(home: Path, creds: Dict[str, Any]) -> bool:
    creds_path = home / ".gemini" / "oauth_creds.json"
    return _write_user_file(creds_path, json.dumps(creds, indent=2) + "\n")


def _refresh_access_token(refresh_token: str, *, timeout: float = 10.0) -> Dict[str, Any]:
    """Exchange a refresh token for a fresh access token.

    Returns the raw JSON response from Google's token endpoint, e.g.
        {"access_token": "...", "expires_in": 3599, "scope": "...",
         "token_type": "Bearer", "id_token": "..."}

    Raises `_HTTPError` on non-2xx.
    """
    return _http_post_form(
        GEMINI_OAUTH_TOKEN_ENDPOINT,
        {
            "client_id": GEMINI_OAUTH_CLIENT_ID,
            "client_secret": GEMINI_OAUTH_CLIENT_SECRET,
            "refresh_token": refresh_token,
            "grant_type": "refresh_token",
        },
        timeout=timeout,
    )


def _is_token_stale(creds: Dict[str, Any], *, now: Optional[float] = None) -> bool:
    """gemini-cli stores `expiry_date` as Unix epoch MILLISECONDS."""
    n = now if now is not None else _now_seconds()
    exp_ms = creds.get("expiry_date")
    if not isinstance(exp_ms, (int, float)):
        return True
    return (exp_ms / 1000.0) <= (n + _REFRESH_LEEWAY_SECONDS)


def _ensure_fresh_token(
    home: Path,
    *,
    refresher=None,
    now: Optional[float] = None,
) -> Optional[str]:
    """Read creds; refresh if stale; persist back; return the access token.

    `refresher` is injected for tests (defaults to `_refresh_access_token`).
    Returns None on any error.
    """
    creds = _load_creds(home)
    if not creds:
        return None
    if not _is_token_stale(creds, now=now):
        return creds.get("access_token")
    refresh_token = creds.get("refresh_token")
    if not refresh_token:
        return None
    refresh_fn = refresher or _refresh_access_token
    try:
        resp = refresh_fn(refresh_token)
    except _HTTPError:
        return None
    except Exception:
        return None
    new_access = resp.get("access_token")
    if not new_access:
        return None
    expires_in = resp.get("expires_in") or 3600
    new_creds = dict(creds)
    new_creds["access_token"] = new_access
    if "id_token" in resp:
        new_creds["id_token"] = resp["id_token"]
    if "token_type" in resp:
        new_creds["token_type"] = resp["token_type"]
    if "scope" in resp:
        new_creds["scope"] = resp["scope"]
    # gemini-cli stores expiry_date in ms; mirror that.
    n_ms = int((now if now is not None else _now_seconds()) * 1000)
    new_creds["expiry_date"] = n_ms + int(expires_in) * 1000
    _save_creds(home, new_creds)
    return new_access


# --------------------------------------------------------------------- projectId cache


def _account_email(home: Path) -> Optional[str]:
    """Read the active gemini account email. Same source the
    `gemini_accounts` module uses (~/.gemini/google_accounts.json -> active).
    Best-effort; returns None on any error.
    """
    raw = _read_user_file(home / ".gemini" / "google_accounts.json")
    if not raw:
        return None
    try:
        d = json.loads(raw)
    except Exception:
        return None
    val = d.get("active")
    if isinstance(val, str) and val:
        return val
    return None


def _project_id_path(home: Path, email: str) -> Path:
    return home / ".gemini" / "accounts" / email / "projectId"


def _read_cached_project_id(home: Path, email: str) -> Optional[str]:
    raw = _read_user_file(_project_id_path(home, email))
    if not raw:
        return None
    s = raw.strip()
    return s or None


def _write_cached_project_id(home: Path, email: str, project_id: str) -> bool:
    parent = _project_id_path(home, email).parent
    if not _mkdir_user(parent):
        return False
    return _write_user_file(_project_id_path(home, email), project_id + "\n")


def _load_code_assist(access_token: str, *, timeout: float = 10.0) -> Optional[str]:
    """Call loadCodeAssist with `cloudaicompanionProject: null` to discover
    the implicit projectId for this account. Returns the projectId or None.
    """
    try:
        resp = _http_post_json(
            CODE_ASSIST_LOAD,
            {"cloudaicompanionProject": None, "metadata": {"pluginType": "GEMINI"}},
            {"Authorization": f"Bearer {access_token}"},
            timeout=timeout,
        )
    except _HTTPError:
        return None
    except Exception:
        return None
    pid = resp.get("cloudaicompanionProject") if isinstance(resp, dict) else None
    if isinstance(pid, str) and pid:
        return pid
    # Some response shapes nest it under `currentTier.cloudaicompanionProject`.
    ct = resp.get("currentTier") if isinstance(resp, dict) else None
    if isinstance(ct, dict):
        pid = ct.get("cloudaicompanionProject")
        if isinstance(pid, str) and pid:
            return pid
    return None


def _resolve_project_id(
    home: Path,
    access_token: str,
    *,
    bust_cache: bool = False,
    loader=None,
) -> Optional[str]:
    """Look up projectId, going through the per-account cache first.

    On `bust_cache=True` (e.g. after a 401/403 from retrieveUserQuota
    that hinted at a stale projectId), skip the cache and force a fresh
    `loadCodeAssist` call.
    """
    email = _account_email(home) or ""
    if not email:
        return None
    if not bust_cache:
        cached = _read_cached_project_id(home, email)
        if cached:
            return cached
    fn = loader or _load_code_assist
    try:
        pid = fn(access_token)
    except Exception:
        pid = None
    if pid:
        _write_cached_project_id(home, email, pid)
    return pid


# --------------------------------------------------------------------- resetTime parsing


def _parse_rfc3339(value: Any) -> Optional[float]:
    """Parse Google RFC 3339 timestamps to a Unix epoch float.

    Accepts e.g. "2026-05-09T15:30:00Z", "2026-05-09T15:30:00.123456Z",
    or with a "+00:00" offset. Returns None on any failure.
    """
    if not isinstance(value, str) or not value:
        return None
    s = value.strip()
    # Python 3.11+ fromisoformat accepts trailing 'Z'; older versions don't.
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    try:
        dt = datetime.fromisoformat(s)
    except Exception:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.timestamp()


def _format_resets_at_repr(resets_epoch: Optional[float]) -> Optional[str]:
    """Format reset epoch like the previous TUI scrape: "3:05 PM".

    Uses local time (matches the gemini /model output the CLI itself shows).
    Returns None when no resets_epoch is provided.
    """
    if resets_epoch is None:
        return None
    try:
        dt = datetime.fromtimestamp(float(resets_epoch))
        # Strip leading zero from the hour for "3:05 PM" not "03:05 PM".
        s = dt.strftime("%I:%M %p")
        return s.lstrip("0") or s
    except Exception:
        return None


# --------------------------------------------------------------------- circuit breaker


def is_suspended() -> bool:
    return _PROVIDER_KEY in _SUSPENDED_PROVIDERS


def _record_outcome(ok: bool) -> None:
    if ok:
        _CONSECUTIVE_FAILURES[_PROVIDER_KEY] = 0
        return
    n = _CONSECUTIVE_FAILURES.get(_PROVIDER_KEY, 0) + 1
    _CONSECUTIVE_FAILURES[_PROVIDER_KEY] = n
    if n >= PROBE_FAILURE_THRESHOLD:
        _SUSPENDED_PROVIDERS.add(_PROVIDER_KEY)


def _reset_circuit_for_tests() -> None:
    """Test helper. Not for production use."""
    _CONSECUTIVE_FAILURES.pop(_PROVIDER_KEY, None)
    _SUSPENDED_PROVIDERS.discard(_PROVIDER_KEY)


# --------------------------------------------------------------------- core retrieve


def _retrieve_user_quota(access_token: str, project_id: str, *, timeout: float = 10.0) -> Dict[str, Any]:
    """Single retrieveUserQuota POST. Raises `_HTTPError` on non-2xx."""
    return _http_post_json(
        CODE_ASSIST_RETRIEVE_USER_QUOTA,
        {"project": project_id},
        {"Authorization": f"Bearer {access_token}"},
        timeout=timeout,
    )


def _normalize_quota_response(payload: Dict[str, Any], *, now: Optional[float] = None) -> Dict[str, dict]:
    """Map a retrieveUserQuota response → {category: {...}} dict.

    Schema returned (matches the dict shape the previous TUI scrape produced
    for `gemini_accounts.check_budget_low` callers):

        {
          "Pro": {
            "remaining_pct": 7.0,    # <-- consumed by gemini_accounts
            "pct_used": 93.0,
            "resets_at": 1715200000.0,
            "resets_at_repr": "3:05 PM",
            "resets_in_seconds": 21600,
            "window_seconds": 86400,
          },
          "Flash": {...},
          "Flash Lite": {...},
        }
    """
    n = now if now is not None else _now_seconds()
    out: Dict[str, dict] = {}
    buckets = payload.get("buckets") if isinstance(payload, dict) else None
    if not isinstance(buckets, list):
        return out
    # If multiple buckets map to the same category, prefer the lowest
    # remaining fraction (most-pessimistic) to avoid masking exhaustion.
    for b in buckets:
        if not isinstance(b, dict):
            continue
        model_id = b.get("modelId")
        category = _category_for_model_id(model_id) if isinstance(model_id, str) else None
        if not category:
            continue
        rf = b.get("remainingFraction")
        try:
            rf_f = float(rf)
        except (TypeError, ValueError):
            continue
        rf_f = max(0.0, min(1.0, rf_f))
        pct_used = round((1.0 - rf_f) * 100, 2)
        remaining_pct = round(rf_f * 100, 2)
        resets_at = _parse_rfc3339(b.get("resetTime"))
        resets_in = int(max(0, resets_at - n)) if resets_at else None
        entry = {
            "remaining_pct": remaining_pct,
            "pct_used": pct_used,
            "resets_at": resets_at,
            "resets_at_repr": _format_resets_at_repr(resets_at),
            "resets_in_seconds": resets_in,
            "window_seconds": _GEMINI_QUOTA_WINDOW_SECONDS,
        }
        existing = out.get(category)
        if (
            existing is None
            or (
                isinstance(existing.get("remaining_pct"), (int, float))
                and remaining_pct < existing["remaining_pct"]
            )
        ):
            out[category] = entry
    return out


# --------------------------------------------------------------------- public entrypoint


def fetch_gemini_user_quota(
    burst_home: Optional[Path] = None,
    *,
    timeout: float = 10.0,
    refresher=None,
    loader=None,
    retriever=None,
    now: Optional[float] = None,
) -> Dict[str, dict]:
    """Fetch per-category Gemini quota via Code Assist HTTPS API.

    Returns a `{category: {remaining_pct, pct_used, resets_at, ...}}` dict.
    Returns an empty dict on any failure (creds missing, refresh failed,
    HTTP error, projectId unresolvable, breaker tripped). NEVER raises.

    `burst_home` must be the per-burst fake-home rooted at
    `<runtime>/burst-homes/<burst_id>/`; raises if None — callers must
    always thread it through post-bwrap-only.

    Test seams:
      - `refresher(refresh_token) -> dict` for `_refresh_access_token`
      - `loader(access_token) -> Optional[str]` for `_load_code_assist`
      - `retriever(access_token, project_id) -> dict` for `_retrieve_user_quota`
    """
    if is_suspended():
        return {}
    if burst_home is None:
        raise ValueError("fetch_gemini_user_quota: burst_home is required (caller bug)")
    home = Path(burst_home).resolve()
    try:
        access_token = _ensure_fresh_token(home, refresher=refresher, now=now)
        if not access_token:
            _record_outcome(False)
            return {}
        project_id = _resolve_project_id(home, access_token, loader=loader)
        if not project_id:
            _record_outcome(False)
            return {}
        retrieve_fn = retriever or _retrieve_user_quota
        try:
            payload = retrieve_fn(access_token, project_id)
        except _HTTPError as e:
            # 401/403 → projectId may be stale (e.g. account rotated, or
            # the cached pid no longer corresponds to a valid Code Assist
            # subscription). Bust the cache and retry once.
            if e.status in (401, 403):
                project_id = _resolve_project_id(
                    home, access_token, bust_cache=True, loader=loader,
                )
                if not project_id:
                    _record_outcome(False)
                    return {}
                try:
                    payload = retrieve_fn(access_token, project_id)
                except Exception:
                    _record_outcome(False)
                    return {}
            else:
                _record_outcome(False)
                return {}
        except Exception:
            _record_outcome(False)
            return {}
        result = _normalize_quota_response(payload, now=now)
        if not result:
            _record_outcome(False)
            return {}
        _record_outcome(True)
        return result
    except Exception:
        try:
            _record_outcome(False)
        except Exception:
            pass
        return {}
