"""Sidecar daemon foundations: config / spool / ledger (plan commit 7)."""

from __future__ import annotations

import json
import time
from pathlib import Path

import pytest

from trellis.sidecar.config import SidecarConfig, read_api_key
from trellis.sidecar.ledger import (
    append_ledger_row,
    budget_allows_new_attempt,
    ledger_path,
    rolling_token_totals,
)
from trellis.sidecar.spool import (
    ensure_spool_dirs,
    iter_terminal_records,
    pending_attempt_ids,
    publish_attempt,
    read_candidates,
    spool_dirs,
    sweep_inflight_to_abandoned,
)


# ---------------------------------------------------------------------------
# config
# ---------------------------------------------------------------------------


def test_config_inert_without_file_or_block(tmp_path: Path) -> None:
    assert SidecarConfig.load(tmp_path / "missing.json").enabled is False
    cfg_path = tmp_path / "trellis.config.json"
    cfg_path.write_text(json.dumps({"worker": {"provider": "codex"}}))
    assert SidecarConfig.load(cfg_path).enabled is False
    cfg_path.write_text("not json {")
    assert SidecarConfig.load(cfg_path).enabled is False


def test_config_parses_full_block(tmp_path: Path) -> None:
    cfg_path = tmp_path / "trellis.config.json"
    cfg_path.write_text(
        json.dumps(
            {
                "sidecar": {
                    "enabled": True,
                    "model": {
                        "provider": "mistral",
                        "name": "labs-leanstral-1-5",
                        "endpoint": "https://api.mistral.ai/v1/chat/completions",
                        "api_key_env_file": "~/.vibe/.env",
                        "api_key_var": "MISTRAL_API_KEY",
                        "tool_calls": True,
                    },
                    "budgets": {
                        "attempt_wall_seconds": 1800,
                        "max_iterations": 12,
                        "attempt_tokens": 500000,
                        "daily_tokens": 25000000,
                        "monthly_tokens": 800000000,
                    },
                    "daemon": {"poll_seconds": 5, "lean_threads": 2},
                }
            }
        )
    )
    cfg = SidecarConfig.load(cfg_path)
    assert cfg.enabled
    assert cfg.model_name == "labs-leanstral-1-5"
    assert cfg.max_iterations == 12
    assert cfg.attempt_tokens == 500_000
    assert cfg.monthly_tokens == 800_000_000
    assert cfg.poll_seconds == 5.0
    assert cfg.lean_threads == 2
    assert cfg.sandbox_role == "lake_compiler"


def test_config_bogus_values_fall_back_to_defaults(tmp_path: Path) -> None:
    cfg_path = tmp_path / "trellis.config.json"
    cfg_path.write_text(
        json.dumps(
            {
                "sidecar": {
                    "enabled": True,
                    "budgets": {"max_iterations": "twelve", "attempt_tokens": -5},
                }
            }
        )
    )
    cfg = SidecarConfig.load(cfg_path)
    # Non-int / negative fall back to the (disabled) defaults.
    assert cfg.max_iterations == 0
    assert cfg.attempt_tokens == 0


def test_config_default_budgets_are_wall_clock_regime() -> None:
    """BENCH_REPORT §7.8: wall-clock is the SOLE per-attempt budget
    (default 90 min — the regime the bench evidence was collected
    under: its decisive close took 64.9 min, where the prior 60-min
    wall would have cut it off ~8% short). Per-attempt token/iteration
    caps and cumulative volume caps are all DISABLED (0). Throughput is
    bounded only by the grunt pool + wall. Compaction fires at the 200k
    live-context threshold (vibe's auto_compact analog)."""
    cfg = SidecarConfig()
    assert cfg.attempt_wall_seconds == 5400.0
    assert cfg.max_iterations == 0
    assert cfg.attempt_tokens == 0
    assert cfg.compact_context_tokens == 200_000
    assert cfg.daily_tokens == 0
    assert cfg.monthly_tokens == 0
    assert cfg.reasoning_effort == "high"


def test_config_top_level_loogle_key(tmp_path: Path) -> None:
    """`loogle.enabled` is a TOP-LEVEL trellis.config.json key (the
    worker-prompt precedent): absent => enabled; explicit false =>
    the search tool is absent."""
    cfg_path = tmp_path / "trellis.config.json"
    cfg_path.write_text(json.dumps({"sidecar": {"enabled": True}}))
    assert SidecarConfig.load(cfg_path).loogle_enabled is True
    cfg_path.write_text(
        json.dumps({"sidecar": {"enabled": True}, "loogle": {"enabled": False}})
    )
    assert SidecarConfig.load(cfg_path).loogle_enabled is False


def test_read_api_key_from_env_file(tmp_path: Path) -> None:
    # Legacy HTTP-arm gate: only a non-codex provider reads the env file
    # (the default provider is codex, which returns a sentinel instead).
    env = tmp_path / "env"
    env.write_text("# comment\nMISTRAL_API_KEY=sk-test-123\n")
    cfg = SidecarConfig(enabled=True, provider="mistral", api_key_env_file=str(env))
    assert read_api_key(cfg) == "sk-test-123"
    # Absent var / absent file => None (daemon suspends, never crashes).
    env.write_text("OTHER=x\n")
    assert read_api_key(cfg) is None
    cfg2 = SidecarConfig(
        enabled=True, provider="mistral", api_key_env_file=str(tmp_path / "nope")
    )
    assert read_api_key(cfg2) is None


# ---------------------------------------------------------------------------
# spool
# ---------------------------------------------------------------------------


def _record(attempt_id: str = "sc-20260722-000001-Rung") -> dict:
    return {
        "schema": 1,
        "attempt_id": attempt_id,
        "node": "Rung",
        "status": "success",
        "artifact": {"proof_body": "  trivial\n"},
    }


def test_publish_attempt_is_rename_only(tmp_path: Path) -> None:
    dirs = spool_dirs(tmp_path)
    ensure_spool_dirs(dirs)
    published = publish_attempt(dirs, _record())
    assert published.parent == dirs.pending
    assert pending_attempt_ids(dirs) == ["sc-20260722-000001-Rung"]
    # Nothing left in inflight; the publish is the atomic rename.
    assert list(dirs.inflight.glob("*.json")) == []
    body = json.loads(published.read_text())
    assert body["artifact"]["proof_body"] == "  trivial\n"


def test_crash_simulation_never_publishes_half_records(tmp_path: Path) -> None:
    dirs = spool_dirs(tmp_path)
    ensure_spool_dirs(dirs)
    # Simulate a crash mid-construction: a file sits in inflight/.
    (dirs.inflight / "attempt-sc-crashed.json").write_text('{"partial":')
    moved = sweep_inflight_to_abandoned(dirs)
    assert [p.name for p in moved] == ["attempt-sc-crashed.json"]
    assert pending_attempt_ids(dirs) == []
    assert (dirs.abandoned / "attempt-sc-crashed.json").exists()


def test_iter_terminal_records_reads_verdicts(tmp_path: Path) -> None:
    dirs = spool_dirs(tmp_path)
    ensure_spool_dirs(dirs)
    applied = _record("sc-a")
    applied["verdict"] = {"outcome": "applied", "reason": "ok", "cycle": 3}
    (dirs.applied / "attempt-sc-a.json").write_text(json.dumps(applied))
    rejected = _record("sc-b")
    rejected["verdict"] = {"outcome": "rejected", "reason": "stale_content"}
    (dirs.rejected / "attempt-sc-b.json").write_text(json.dumps(rejected))
    (dirs.rejected / "attempt-sc-poison.json").write_text("{broken")

    records = list(iter_terminal_records(dirs))
    outcomes = {r["attempt_id"]: r["verdict"]["outcome"] for r in records}
    assert outcomes == {"sc-a": "applied", "sc-b": "rejected"}


def test_read_candidates_absent_or_malformed(tmp_path: Path) -> None:
    assert read_candidates(tmp_path) is None
    path = tmp_path / "sidecar" / "candidates.json"
    path.parent.mkdir(parents=True)
    path.write_text("nope{")
    assert read_candidates(tmp_path) is None
    path.write_text(json.dumps({"schema": 1, "candidates": []}))
    data = read_candidates(tmp_path)
    assert data is not None and data["schema"] == 1


# ---------------------------------------------------------------------------
# ledger
# ---------------------------------------------------------------------------


def test_ledger_append_and_rolling_totals(tmp_path: Path) -> None:
    now = time.time()
    append_ledger_row(
        tmp_path,
        {
            "node": "Rung",
            "status": "success",
            "prompt_tokens": 1000,
            "completion_tokens": 200,
        },
    )
    # An old row (35 days) counts toward neither window.
    old = {
        "ts": now - 35 * 24 * 3600,
        "node": "Old",
        "prompt_tokens": 7_000_000,
        "completion_tokens": 0,
    }
    with ledger_path(tmp_path).open("a") as handle:
        handle.write(json.dumps(old) + "\n")
        handle.write("garbage line\n")
    # A mid-month row counts toward month only.
    mid = {"ts": now - 5 * 24 * 3600, "prompt_tokens": 500, "completion_tokens": 0}
    with ledger_path(tmp_path).open("a") as handle:
        handle.write(json.dumps(mid) + "\n")

    totals = rolling_token_totals(tmp_path, now=time.time() + 1)
    assert totals.day_tokens == 1200
    assert totals.month_tokens == 1700


def test_ledger_budget_caps_refuse_new_attempts(tmp_path: Path) -> None:
    now = time.time()
    assert budget_allows_new_attempt(tmp_path, 100, 1000, now=now)
    append_ledger_row(tmp_path, {"prompt_tokens": 90, "completion_tokens": 20})
    assert not budget_allows_new_attempt(tmp_path, 100, 1000, now=now + 1)
    # Daily window rolls off; monthly cap still binds if exceeded.
    later = now + 2 * 24 * 3600
    assert budget_allows_new_attempt(tmp_path, 100, 1000, now=later)
    assert not budget_allows_new_attempt(tmp_path, 100, 110, now=later)


def test_ledger_caps_disabled_never_gate(tmp_path: Path) -> None:
    """Wall-clock regime: caps of 0 are DISABLED — cumulative volume
    never gates a new attempt, no matter how large the ledger. The
    default config (daily=0, monthly=0) therefore always admits."""
    now = time.time()
    append_ledger_row(tmp_path, {"prompt_tokens": 10**12, "completion_tokens": 0})
    # Both caps off: always allowed, even with a trillion tokens logged.
    assert budget_allows_new_attempt(tmp_path, 0, 0, now=now + 1)
    # A disabled cap is ignored while a positive one still binds.
    assert budget_allows_new_attempt(tmp_path, 0, 10**15, now=now + 1)
    assert not budget_allows_new_attempt(tmp_path, 10**6, 0, now=now + 1)


def test_ledger_append_never_raises(tmp_path: Path) -> None:
    # Point the ledger at an unwritable location: append must swallow.
    blocked = tmp_path / "sidecar"
    blocked.write_text("a file where the dir should be")
    append_ledger_row(tmp_path, {"node": "X"})  # no raise
    assert rolling_token_totals(tmp_path).month_tokens == 0


def test_publish_outcome_is_rename_only_into_its_own_lane(tmp_path: Path) -> None:
    """The spent-generation lane follows the same ownership protocol as
    `pending/`: write into the daemon's private `inflight/`, publish by
    one atomic rename, never touch the kernel-owned dirs."""
    from trellis.sidecar.spool import publish_outcome

    dirs = spool_dirs(tmp_path)
    ensure_spool_dirs(dirs)
    published = publish_outcome(
        dirs,
        {
            "schema": 1,
            "attempt_id": "sc-20260722-000001-Rung",
            "node": "Rung",
            "entry_seq": 4,
            "status": "failed",
            "detail": "unsolved goals",
            "export_cycle": 17,
        },
    )
    assert published.parent == dirs.outcomes
    assert published.name == "outcome-sc-20260722-000001-Rung.json"
    assert list(dirs.inflight.glob("*.json")) == []
    # The closure lane is untouched — the two lanes never collide, even
    # for the same attempt id.
    assert list(dirs.pending.glob("*.json")) == []
    body = json.loads(published.read_text())
    assert body["entry_seq"] == 4 and body["export_cycle"] == 17


def test_publish_outcome_requires_an_attempt_id(tmp_path: Path) -> None:
    from trellis.sidecar.spool import publish_outcome

    dirs = spool_dirs(tmp_path)
    ensure_spool_dirs(dirs)
    with pytest.raises(ValueError):
        publish_outcome(dirs, {"node": "Rung"})


def test_inflight_sweep_covers_half_written_outcomes(tmp_path: Path) -> None:
    dirs = spool_dirs(tmp_path)
    ensure_spool_dirs(dirs)
    (dirs.inflight / "outcome-sc-crashed.json").write_text('{"partial":')
    moved = sweep_inflight_to_abandoned(dirs)
    assert [p.name for p in moved] == ["outcome-sc-crashed.json"]
    assert list(dirs.outcomes.glob("*.json")) == []


# ---------------------------------------------------------------------
# `reviewer_export_path` fallback must be VERIFIED, not assumed.
#
# The fallback to `candidates.json` is justified by "a kernel too old to
# write the redacted copy is too old to HAVE a kernel lane". That is
# false when the kernel binary is deployed ahead of the Python tree —
# this project's normal deploy order — and selecting on existence alone
# then binds the unredacted export into the reviewer sandbox.


def _sidecar_dir(tmp_path):
    d = tmp_path / "sidecar"
    d.mkdir(parents=True, exist_ok=True)
    return d


def test_reviewer_export_prefers_the_redacted_copy(tmp_path):
    from trellis.sidecar.spool import reviewer_export_path

    d = _sidecar_dir(tmp_path)
    (d / "reviewer_candidates.json").write_text('{"schema": 3}')
    (d / "candidates.json").write_text('{"schema": 3, "kernel_queue": []}')
    assert reviewer_export_path(tmp_path).name == "reviewer_candidates.json"


def test_a_schema2_export_is_a_legitimate_fallback(tmp_path):
    """A genuinely old kernel has no lane, so its export is safe."""
    from trellis.sidecar.spool import reviewer_export_path

    d = _sidecar_dir(tmp_path)
    (d / "candidates.json").write_text('{"schema": 2, "queue": []}')
    assert reviewer_export_path(tmp_path).name == "candidates.json"


def test_a_lane_bearing_export_is_never_fallen_back_to(tmp_path):
    """The regression: kernel deployed ahead of Python."""
    from trellis.sidecar.spool import reviewer_export_path

    d = _sidecar_dir(tmp_path)
    (d / "candidates.json").write_text('{"schema": 3, "kernel_queue": [{"node": "X"}]}')
    assert reviewer_export_path(tmp_path).name == "reviewer_candidates.json"


def test_schema_3_without_the_key_is_still_refused(tmp_path):
    """`kernel_queue` is skip-when-empty, so absence is not proof."""
    from trellis.sidecar.spool import reviewer_export_path

    d = _sidecar_dir(tmp_path)
    (d / "candidates.json").write_text('{"schema": 3, "queue": []}')
    assert reviewer_export_path(tmp_path).name == "reviewer_candidates.json"


def test_an_unparseable_export_fails_closed(tmp_path):
    from trellis.sidecar.spool import reviewer_export_path

    d = _sidecar_dir(tmp_path)
    (d / "candidates.json").write_text("{ truncated mid-write")
    assert reviewer_export_path(tmp_path).name == "reviewer_candidates.json"


def test_nothing_written_yet_still_yields_a_stable_path(tmp_path):
    from trellis.sidecar.spool import reviewer_export_path

    _sidecar_dir(tmp_path)
    assert reviewer_export_path(tmp_path).name == "reviewer_candidates.json"
