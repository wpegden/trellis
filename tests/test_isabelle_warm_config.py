"""Tests for ``trellis.checker.isabelle_warm_config`` (Phase 1).

The warm-prefix capability is OFF by default; the config object must default to
disabled and stay inert when the ``isabelle_warm_session`` object is missing
from ``trellis.config.json`` (mirrors the ``active_node_prewarm`` "inert without
config" precedent). The env switch the low-level session reads must also default
OFF.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from trellis.checker.isabelle_warm_config import (
    CROSS_CHECK_ENV,
    DEFAULT_CONSOLIDATE_DELAY,
    DEFAULT_CROSS_CHECK_CADENCE,
    FORCE_CROSS_CHECK_ENV,
    IsabelleWarmSessionConfig,
    WARM_SESSION_ENV,
    env_cross_check_cadence,
    env_force_cross_check,
    env_warm_session_enabled,
)


def test_default_config_disabled() -> None:
    cfg = IsabelleWarmSessionConfig()
    assert cfg.enabled is False
    assert cfg.consolidate_delay == DEFAULT_CONSOLIDATE_DELAY


def test_missing_file_is_disabled(tmp_path: Path) -> None:
    """A missing config file => default (disabled), so the capability is inert
    without explicit opt-in."""
    cfg = IsabelleWarmSessionConfig.load(tmp_path / "nope.json")
    assert cfg.enabled is False


def test_missing_object_is_disabled(tmp_path: Path) -> None:
    """A config.json with no ``isabelle_warm_session`` object => disabled."""
    p = tmp_path / "trellis.config.json"
    p.write_text(json.dumps({"active_node_prewarm": {"enabled": True}}), encoding="utf-8")
    cfg = IsabelleWarmSessionConfig.load(p)
    assert cfg.enabled is False


def test_enabled_with_custom_delay(tmp_path: Path) -> None:
    p = tmp_path / "trellis.config.json"
    p.write_text(
        json.dumps({"isabelle_warm_session": {"enabled": True, "consolidate_delay": 0.2}}),
        encoding="utf-8",
    )
    cfg = IsabelleWarmSessionConfig.load(p)
    assert cfg.enabled is True
    assert cfg.consolidate_delay == 0.2


def test_non_positive_delay_falls_back_to_default() -> None:
    """0.0 risks a premature-FAILED false-fast and a negative is nonsense; both
    fall back to the tuned default rather than being honored."""
    assert IsabelleWarmSessionConfig.from_mapping(
        {"enabled": True, "consolidate_delay": 0.0}
    ).consolidate_delay == DEFAULT_CONSOLIDATE_DELAY
    assert IsabelleWarmSessionConfig.from_mapping(
        {"enabled": True, "consolidate_delay": -1}
    ).consolidate_delay == DEFAULT_CONSOLIDATE_DELAY
    assert IsabelleWarmSessionConfig.from_mapping(
        {"consolidate_delay": "garbage"}
    ).consolidate_delay == DEFAULT_CONSOLIDATE_DELAY


def test_malformed_json_is_disabled(tmp_path: Path) -> None:
    p = tmp_path / "trellis.config.json"
    p.write_text("{ not json", encoding="utf-8")
    assert IsabelleWarmSessionConfig.load(p).enabled is False


def test_env_switch_default_off(monkeypatch) -> None:
    monkeypatch.delenv(WARM_SESSION_ENV, raising=False)
    assert env_warm_session_enabled() is False


@pytest.mark.parametrize("val,expected", [
    ("1", True), ("true", True), ("TRUE", True), ("yes", True), ("on", True),
    ("0", False), ("false", False), ("", False), ("nope", False),
])
def test_env_switch_truthy_spellings(monkeypatch, val, expected) -> None:
    monkeypatch.setenv(WARM_SESSION_ENV, val)
    assert env_warm_session_enabled() is expected


# ----------------------- Phase 2: cross-check cadence -----------------------


def test_default_cadence() -> None:
    cfg = IsabelleWarmSessionConfig()
    assert cfg.cross_check_cadence == DEFAULT_CROSS_CHECK_CADENCE
    assert cfg.effective_cross_check_cadence() == DEFAULT_CROSS_CHECK_CADENCE


def test_cadence_from_config(tmp_path: Path) -> None:
    p = tmp_path / "trellis.config.json"
    p.write_text(
        json.dumps({"isabelle_warm_session": {"enabled": True, "cross_check_cadence": 7}}),
        encoding="utf-8",
    )
    cfg = IsabelleWarmSessionConfig.load(p)
    assert cfg.cross_check_cadence == 7


def test_cadence_zero_disables_periodic_but_is_kept() -> None:
    """0 is a legitimate "disable the periodic cadence" value, kept verbatim
    (the anomaly→cold fallback + a forced cross-check still apply)."""
    assert IsabelleWarmSessionConfig.from_mapping(
        {"enabled": True, "cross_check_cadence": 0}
    ).cross_check_cadence == 0


def test_negative_cadence_falls_back_to_default() -> None:
    assert IsabelleWarmSessionConfig.from_mapping(
        {"cross_check_cadence": -5}
    ).cross_check_cadence == DEFAULT_CROSS_CHECK_CADENCE
    assert IsabelleWarmSessionConfig.from_mapping(
        {"cross_check_cadence": "garbage"}
    ).cross_check_cadence == DEFAULT_CROSS_CHECK_CADENCE


def test_cadence_env_overrides_config(monkeypatch) -> None:
    cfg = IsabelleWarmSessionConfig(cross_check_cadence=25)
    monkeypatch.delenv(CROSS_CHECK_ENV, raising=False)
    assert cfg.effective_cross_check_cadence() == 25
    monkeypatch.setenv(CROSS_CHECK_ENV, "3")
    assert cfg.effective_cross_check_cadence() == 3
    # 0 via env disables the periodic cadence.
    monkeypatch.setenv(CROSS_CHECK_ENV, "0")
    assert cfg.effective_cross_check_cadence() == 0
    # Garbage / negative env → config value stays in force.
    monkeypatch.setenv(CROSS_CHECK_ENV, "-1")
    assert cfg.effective_cross_check_cadence() == 25
    monkeypatch.setenv(CROSS_CHECK_ENV, "xyz")
    assert cfg.effective_cross_check_cadence() == 25


def test_env_cross_check_cadence_unset_is_none(monkeypatch) -> None:
    monkeypatch.delenv(CROSS_CHECK_ENV, raising=False)
    assert env_cross_check_cadence() is None


def test_force_cross_check_env_default_off(monkeypatch) -> None:
    monkeypatch.delenv(FORCE_CROSS_CHECK_ENV, raising=False)
    assert env_force_cross_check() is False


@pytest.mark.parametrize("val,expected", [
    ("1", True), ("true", True), ("on", True), ("0", False), ("", False),
])
def test_force_cross_check_env_spellings(monkeypatch, val, expected) -> None:
    monkeypatch.setenv(FORCE_CROSS_CHECK_ENV, val)
    assert env_force_cross_check() is expected
