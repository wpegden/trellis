from __future__ import annotations

from pathlib import Path

from trellis import gemini_accounts


def test_gemini_api_env_keys_to_forward_suppresses_keys_when_accounts_present(
    tmp_path: Path,
) -> None:
    accounts_dir = tmp_path / ".gemini" / "accounts" / "a@example.com"
    accounts_dir.mkdir(parents=True)
    (tmp_path / ".gemini" / "accounts" / "b@example.com").mkdir(parents=True)

    assert gemini_accounts.gemini_api_env_keys_to_forward(
        burst_home=tmp_path,
    ) == ()


def test_ensure_budget_rotates_to_healthy_alt_account(monkeypatch) -> None:
    switch_calls: list[str] = []
    budget_results = iter(
        [
            (True, {"gemini-3.1-pro-preview": {"remaining_pct": 5}}),
            (False, {"gemini-3.1-pro-preview": {"remaining_pct": 60}}),
        ]
    )

    monkeypatch.setattr(
        gemini_accounts,
        "rotation_available",
        lambda **kwargs: True,
    )
    monkeypatch.setattr(
        gemini_accounts,
        "active_account",
        lambda **kwargs: "a@example.com",
    )
    monkeypatch.setattr(
        gemini_accounts,
        "available_accounts",
        lambda **kwargs: ["a@example.com", "b@example.com"],
    )
    monkeypatch.setattr(
        gemini_accounts,
        "check_budget_low",
        lambda **kwargs: next(budget_results),
    )
    monkeypatch.setattr(
        gemini_accounts,
        "switch_account",
        lambda email, **kwargs: switch_calls.append(email) or True,
    )

    active = gemini_accounts.ensure_budget(
        primary_account=None,
    )

    assert active == "b@example.com"
    assert switch_calls == ["b@example.com"]


def test_maybe_ensure_budget_respects_cooldown(monkeypatch, tmp_path: Path) -> None:
    calls: list[str] = []
    monkeypatch.setattr(
        gemini_accounts,
        "rotation_available",
        lambda **kwargs: True,
    )
    monkeypatch.setattr(
        gemini_accounts,
        "active_account",
        lambda **kwargs: "a@example.com",
    )
    monkeypatch.setattr(
        gemini_accounts,
        "ensure_budget",
        lambda **kwargs: calls.append("ensure") or "a@example.com",
    )
    monkeypatch.setattr(gemini_accounts, "BUDGET_CHECK_COOLDOWN_SECONDS", 1000.0)
    monkeypatch.setattr(gemini_accounts, "_last_budget_check_by_home", {})

    gemini_accounts.maybe_ensure_budget(burst_home=tmp_path)
    gemini_accounts.maybe_ensure_budget(burst_home=tmp_path)

    assert calls == ["ensure"]


def test_ensure_budget_falls_back_to_default_primary_when_stats_unavailable(
    monkeypatch,
) -> None:
    switch_calls: list[str] = []

    monkeypatch.setattr(
        gemini_accounts,
        "rotation_available",
        lambda **kwargs: True,
    )
    monkeypatch.setattr(
        gemini_accounts,
        "active_account",
        lambda **kwargs: "secondary@example.com",
    )
    monkeypatch.setattr(
        gemini_accounts,
        "available_accounts",
        lambda **kwargs: ["primary@example.com", "secondary@example.com"],
    )
    monkeypatch.setattr(
        gemini_accounts,
        "check_budget_low",
        lambda **kwargs: (False, {}),
    )
    monkeypatch.setattr(
        gemini_accounts,
        "switch_account",
        lambda email, **kwargs: switch_calls.append(email) or True,
    )

    active = gemini_accounts.ensure_budget(
        primary_account=None,
    )

    assert active == "primary@example.com"
    assert switch_calls == ["primary@example.com"]


