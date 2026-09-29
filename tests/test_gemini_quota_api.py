"""Unit tests for `trellis.gemini_quota_api`.

Network calls are NEVER made — every refresh / loadCodeAssist /
retrieveUserQuota path is monkeypatched. Real calls would consume the
live run's quota and rotate tokens unexpectedly.
"""

from __future__ import annotations

import json
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

import pytest

from trellis import gemini_quota_api as gqa


# --------------------------------------------------------------------- helpers


def _seed_creds(home: Path, *, expiry_ms: int, refresh_token: str = "rt-1", access_token: str = "at-1") -> None:
    (home / ".gemini").mkdir(parents=True, exist_ok=True)
    creds = {
        "access_token": access_token,
        "refresh_token": refresh_token,
        "token_type": "Bearer",
        "scope": "openid",
        "expiry_date": expiry_ms,
    }
    (home / ".gemini" / "oauth_creds.json").write_text(json.dumps(creds), encoding="utf-8")


def _seed_active_email(home: Path, email: str) -> None:
    (home / ".gemini").mkdir(parents=True, exist_ok=True)
    (home / ".gemini" / "google_accounts.json").write_text(
        json.dumps({"active": email, "old": []}), encoding="utf-8"
    )


def _seed_project_id(home: Path, email: str, pid: str) -> None:
    d = home / ".gemini" / "accounts" / email
    d.mkdir(parents=True, exist_ok=True)
    (d / "projectId").write_text(pid + "\n", encoding="utf-8")


@pytest.fixture(autouse=True)
def _reset_breaker():
    gqa._reset_circuit_for_tests()
    yield
    gqa._reset_circuit_for_tests()


# --------------------------------------------------------------------- modelId mapping


def test_category_for_model_id_pro_flash_lite():
    assert gqa._category_for_model_id("gemini-2.5-pro") == "Pro"
    assert gqa._category_for_model_id("gemini-2.5-flash") == "Flash"
    assert gqa._category_for_model_id("gemini-2.5-flash-lite") == "Flash Lite"
    assert gqa._category_for_model_id("gemini-2.5-pro-preview") == "Pro"
    # Lite must beat plain flash (substring would otherwise hit "flash" first).
    assert gqa._category_for_model_id("gemini-2.5-flash-lite-preview") == "Flash Lite"


def test_category_for_model_id_skips_unknown():
    assert gqa._category_for_model_id("gemini-2.5-flash-image") is None
    assert gqa._category_for_model_id("imagen-3.0") is None
    assert gqa._category_for_model_id("") is None
    assert gqa._category_for_model_id(None) is None  # type: ignore[arg-type]


# --------------------------------------------------------------------- RFC 3339 parsing


def test_parse_rfc3339_z_suffix_and_offset():
    # Both forms must round-trip to the same epoch.
    a = gqa._parse_rfc3339("2026-05-09T15:30:00Z")
    b = gqa._parse_rfc3339("2026-05-09T15:30:00+00:00")
    assert a is not None and b is not None
    assert abs(a - b) < 0.001


def test_parse_rfc3339_with_microseconds():
    epoch = gqa._parse_rfc3339("2026-05-09T15:30:00.123456Z")
    assert epoch is not None
    # Microseconds must be honored.
    fractional = epoch - int(epoch)
    assert fractional > 0


def test_parse_rfc3339_garbage_returns_none():
    assert gqa._parse_rfc3339("") is None
    assert gqa._parse_rfc3339(None) is None  # type: ignore[arg-type]
    assert gqa._parse_rfc3339("not-a-date") is None
    assert gqa._parse_rfc3339(12345) is None  # type: ignore[arg-type]


# --------------------------------------------------------------------- token refresh


def test_token_refresh_when_stale(tmp_path: Path):
    now = 2_000_000_000.0
    # Token expired 10s ago.
    _seed_creds(tmp_path, expiry_ms=int((now - 10) * 1000))
    _seed_active_email(tmp_path, "user@example.com")
    _seed_project_id(tmp_path, "user@example.com", "proj-123")

    refresh_calls: List[str] = []

    def fake_refresher(refresh_token: str) -> Dict[str, Any]:
        refresh_calls.append(refresh_token)
        return {"access_token": "at-NEW", "expires_in": 3600, "token_type": "Bearer"}

    retrieve_calls: List[Dict[str, Any]] = []

    def fake_retriever(access_token: str, project_id: str) -> Dict[str, Any]:
        retrieve_calls.append({"token": access_token, "project": project_id})
        return {
            "buckets": [
                {"modelId": "gemini-2.5-pro", "remainingFraction": 0.07,
                 "resetTime": "2026-05-09T15:00:00Z"},
            ]
        }

    out = gqa.fetch_gemini_user_quota(
        burst_home=tmp_path,        refresher=fake_refresher, retriever=fake_retriever, now=now,
    )

    assert refresh_calls == ["rt-1"], "refresher should fire when token is stale"
    assert retrieve_calls and retrieve_calls[0]["token"] == "at-NEW"
    assert "Pro" in out
    assert out["Pro"]["remaining_pct"] == 7.0
    assert out["Pro"]["pct_used"] == 93.0
    assert out["Pro"]["window_seconds"] == 86400

    # Persisted creds should now have the new access token + bumped expiry.
    persisted = json.loads((tmp_path / ".gemini" / "oauth_creds.json").read_text())
    assert persisted["access_token"] == "at-NEW"
    assert persisted["expiry_date"] >= int((now + 3000) * 1000)


def test_token_not_refreshed_when_fresh(tmp_path: Path):
    now = 2_000_000_000.0
    # Token expires in 1 hour; well outside the 60s leeway.
    _seed_creds(tmp_path, expiry_ms=int((now + 3600) * 1000))
    _seed_active_email(tmp_path, "user@example.com")
    _seed_project_id(tmp_path, "user@example.com", "proj-123")

    def boom_refresher(_: str) -> Dict[str, Any]:
        raise AssertionError("refresher must NOT be called when token is fresh")

    def fake_retriever(access_token: str, project_id: str) -> Dict[str, Any]:
        assert access_token == "at-1"
        return {"buckets": [{"modelId": "gemini-2.5-flash", "remainingFraction": 0.5,
                             "resetTime": "2026-05-09T22:00:00Z"}]}

    out = gqa.fetch_gemini_user_quota(
        burst_home=tmp_path,        refresher=boom_refresher, retriever=fake_retriever, now=now,
    )
    assert out["Flash"]["remaining_pct"] == 50.0
    assert out["Flash"]["pct_used"] == 50.0


# --------------------------------------------------------------------- projectId cache bust on 401


def test_project_id_cache_bust_on_401(tmp_path: Path):
    now = 2_000_000_000.0
    _seed_creds(tmp_path, expiry_ms=int((now + 3600) * 1000))
    _seed_active_email(tmp_path, "user@example.com")
    _seed_project_id(tmp_path, "user@example.com", "STALE-PROJECT")

    loader_calls: List[Optional[bool]] = []

    def fake_loader(_token: str) -> str:
        loader_calls.append(True)
        return "FRESH-PROJECT"

    retrieve_calls: List[str] = []

    def fake_retriever(_access_token: str, project_id: str) -> Dict[str, Any]:
        retrieve_calls.append(project_id)
        if project_id == "STALE-PROJECT":
            raise gqa._HTTPError(401, "stale project")
        return {
            "buckets": [
                {"modelId": "gemini-2.5-pro", "remainingFraction": 0.42,
                 "resetTime": "2026-05-10T01:00:00Z"},
            ]
        }

    out = gqa.fetch_gemini_user_quota(
        burst_home=tmp_path,        loader=fake_loader, retriever=fake_retriever, now=now,
    )
    assert retrieve_calls == ["STALE-PROJECT", "FRESH-PROJECT"]
    assert loader_calls == [True], "loader must fire exactly once on 401"
    assert out["Pro"]["remaining_pct"] == 42.0

    # Cache file must have been rewritten with the fresh pid.
    cached = (tmp_path / ".gemini" / "accounts" / "user@example.com" / "projectId").read_text()
    assert cached.strip() == "FRESH-PROJECT"


def test_project_id_loaded_when_cache_missing(tmp_path: Path):
    now = 2_000_000_000.0
    _seed_creds(tmp_path, expiry_ms=int((now + 3600) * 1000))
    _seed_active_email(tmp_path, "user@example.com")
    # No projectId file — loader must populate it.

    def fake_loader(_token: str) -> str:
        return "DISCOVERED-PROJECT"

    def fake_retriever(_access_token: str, project_id: str) -> Dict[str, Any]:
        assert project_id == "DISCOVERED-PROJECT"
        return {"buckets": [{"modelId": "gemini-2.5-flash-lite", "remainingFraction": 1.0,
                             "resetTime": "2026-05-09T20:00:00Z"}]}

    out = gqa.fetch_gemini_user_quota(
        burst_home=tmp_path,        loader=fake_loader, retriever=fake_retriever, now=now,
    )
    assert "Flash Lite" in out
    assert out["Flash Lite"]["remaining_pct"] == 100.0
    assert out["Flash Lite"]["pct_used"] == 0.0
    cached = (tmp_path / ".gemini" / "accounts" / "user@example.com" / "projectId").read_text()
    assert cached.strip() == "DISCOVERED-PROJECT"


# --------------------------------------------------------------------- normalize


def test_normalize_quota_response_full_shape():
    now = 2_000_000_000.0
    payload = {
        "buckets": [
            {"modelId": "gemini-2.5-pro", "remainingFraction": 0.07,
             "resetTime": "2026-05-09T15:30:00Z"},
            {"modelId": "gemini-2.5-flash", "remainingFraction": 0.5,
             "resetTime": "2026-05-09T22:00:00Z"},
            {"modelId": "gemini-2.5-flash-lite", "remainingFraction": 0.99,
             "resetTime": "2026-05-09T20:00:00Z"},
            # Unknown model should be silently dropped.
            {"modelId": "imagen-3.0", "remainingFraction": 0.0,
             "resetTime": "2026-05-09T20:00:00Z"},
        ]
    }
    out = gqa._normalize_quota_response(payload, now=now)
    assert set(out) == {"Pro", "Flash", "Flash Lite"}

    pro = out["Pro"]
    assert pro["remaining_pct"] == 7.0
    assert pro["pct_used"] == 93.0
    assert pro["window_seconds"] == 86400
    assert pro["resets_at"] is not None
    assert pro["resets_in_seconds"] is not None and pro["resets_in_seconds"] >= 0
    assert isinstance(pro["resets_at_repr"], str) and pro["resets_at_repr"]


def test_normalize_quota_response_empty_buckets():
    assert gqa._normalize_quota_response({}, now=2_000_000_000.0) == {}
    assert gqa._normalize_quota_response({"buckets": []}, now=2_000_000_000.0) == {}
    assert gqa._normalize_quota_response({"buckets": "not-a-list"}, now=2_000_000_000.0) == {}


def test_normalize_quota_response_pessimistic_when_duplicate_category():
    # Two pro buckets — keep the one with lower remainingFraction.
    payload = {
        "buckets": [
            {"modelId": "gemini-2.5-pro", "remainingFraction": 0.5,
             "resetTime": "2026-05-09T15:30:00Z"},
            {"modelId": "gemini-2.5-pro-preview", "remainingFraction": 0.05,
             "resetTime": "2026-05-09T15:30:00Z"},
        ]
    }
    out = gqa._normalize_quota_response(payload, now=2_000_000_000.0)
    assert out["Pro"]["remaining_pct"] == 5.0


# --------------------------------------------------------------------- circuit breaker


def test_circuit_breaker_after_three_failures(tmp_path: Path):
    now = 2_000_000_000.0
    _seed_creds(tmp_path, expiry_ms=int((now + 3600) * 1000))
    _seed_active_email(tmp_path, "user@example.com")
    _seed_project_id(tmp_path, "user@example.com", "proj-1")

    retrieve_call_count = {"n": 0}

    def explosive_retriever(_token: str, _pid: str) -> Dict[str, Any]:
        retrieve_call_count["n"] += 1
        raise gqa._HTTPError(500, "boom")

    # First 3 calls all fail; circuit breaker should now be tripped.
    for _ in range(3):
        out = gqa.fetch_gemini_user_quota(
        burst_home=tmp_path,            retriever=explosive_retriever, now=now,
        )
        assert out == {}

    assert gqa.is_suspended(), "breaker should be tripped after 3 failures"
    assert retrieve_call_count["n"] == 3

    # 4th call must short-circuit BEFORE invoking the retriever.
    out = gqa.fetch_gemini_user_quota(
        burst_home=tmp_path,        retriever=explosive_retriever, now=now,
    )
    assert out == {}
    assert retrieve_call_count["n"] == 3, "no retriever call once suspended"


def test_circuit_breaker_resets_on_success(tmp_path: Path):
    now = 2_000_000_000.0
    _seed_creds(tmp_path, expiry_ms=int((now + 3600) * 1000))
    _seed_active_email(tmp_path, "user@example.com")
    _seed_project_id(tmp_path, "user@example.com", "proj-1")

    state = {"fail": True}

    def flaky_retriever(_token: str, _pid: str) -> Dict[str, Any]:
        if state["fail"]:
            raise gqa._HTTPError(500, "boom")
        return {"buckets": [{"modelId": "gemini-2.5-pro", "remainingFraction": 0.3,
                             "resetTime": "2026-05-09T20:00:00Z"}]}

    # 2 failures, then success — breaker should be cleared.
    for _ in range(2):
        gqa.fetch_gemini_user_quota(
        burst_home=tmp_path,            retriever=flaky_retriever, now=now,
        )
    state["fail"] = False
    out = gqa.fetch_gemini_user_quota(
        burst_home=tmp_path,        retriever=flaky_retriever, now=now,
    )
    assert "Pro" in out
    assert not gqa.is_suspended()
    assert gqa._CONSECUTIVE_FAILURES.get(gqa._PROVIDER_KEY, 0) == 0


# --------------------------------------------------------------------- error paths


def test_no_creds_returns_empty(tmp_path: Path):
    # Don't seed creds — fetch must return empty without raising.
    out = gqa.fetch_gemini_user_quota(burst_home=tmp_path, now=1.0)
    assert out == {}


def test_refresher_failure_returns_empty(tmp_path: Path):
    now = 2_000_000_000.0
    _seed_creds(tmp_path, expiry_ms=int((now - 10) * 1000))
    _seed_active_email(tmp_path, "user@example.com")
    _seed_project_id(tmp_path, "user@example.com", "proj-1")

    def busted_refresher(_: str) -> Dict[str, Any]:
        raise gqa._HTTPError(400, "bad refresh token")

    out = gqa.fetch_gemini_user_quota(
        burst_home=tmp_path,        refresher=busted_refresher, now=now,
    )
    assert out == {}


def test_no_project_id_returns_empty(tmp_path: Path):
    now = 2_000_000_000.0
    _seed_creds(tmp_path, expiry_ms=int((now + 3600) * 1000))
    _seed_active_email(tmp_path, "user@example.com")

    # Loader returns nothing — projectId can't be resolved.
    out = gqa.fetch_gemini_user_quota(
        burst_home=tmp_path,        loader=lambda _t: None, now=now,
    )
    assert out == {}
