"""`codex_timing model-mix`: does a burst on the wrong model get SEEN?

The failure this answers: a run configured for gpt-5.6-sol dispatched 107
of 141 reviewer-role bursts on gpt-5.5 across 34 cycles (~$91) before a
human noticed by eye. Nothing was broken — the ledger had `role`, `model`
and `scope` on every row the whole time. What was missing was any surface
that asked the question, and a role tally could not: all three verifier
pools log as role "reviewer", so the wrong-model bursts sat inside a
"reviewer" bucket that looked entirely normal.

So the property under test is not "the aggregation sums correctly" but
"a model no lane in the config names is reported as unexpected, and one
that IS named is not" — plus the two ways such a check goes wrong in
practice: crying wolf on a config it cannot read, and being unable to
tell a lane that is wrong NOW from a run that was legitimately upgraded
months ago.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

from trellis.codex_timing import config_models

REPO_ROOT = Path(__file__).resolve().parents[1]


def write_ledger(path: Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")


def row(model: str, scope: str, role: str = "reviewer", ts: float = 1_787_900_000.0,
        cost: float = 1.0) -> dict:
    return {"role": role, "model": model, "scope": scope, "ts": ts,
            "cost_usd": cost, "usage": {"model": model}}


def run_mix(ledger: Path, *extra: str) -> tuple[int, dict]:
    proc = subprocess.run(
        [sys.executable, "-m", "trellis.codex_timing", "model-mix",
         "--ledger", str(ledger), *extra],
        capture_output=True, text=True, timeout=60, cwd=REPO_ROOT,
    )
    return proc.returncode, json.loads(proc.stdout)


def make_run(tmp_path: Path, config: dict, rows: list[dict]) -> Path:
    """A minimal run tree: <repo>/trellis.config.json + the ledger."""
    repo = tmp_path / "run"
    (repo / ".trellis" / "logs").mkdir(parents=True, exist_ok=True)
    (repo / "trellis.config.json").write_text(json.dumps(config), encoding="utf-8")
    ledger = repo / ".trellis" / "logs" / "cost-ledger.jsonl"
    write_ledger(ledger, rows)
    return ledger


CONFIG_SOL = {
    "worker": {"provider": "codex", "model": "gpt-5.6-sol"},
    "reviewer": {"provider": "codex", "model": "gpt-5.6-sol"},
    "verification": {
        "correspondence_agents": [{"provider": "codex", "model": "gpt-5.6-sol"}],
        "soundness_agents": [{"provider": "codex", "model": "gpt-5.6-sol"}],
        "substantiveness_agents": [{"provider": "codex", "model": "gpt-5.6-sol"}],
    },
}


def test_config_models_walks_nested_pools_and_the_sidecar() -> None:
    """The expected-set must come from a generic walk.

    A hand-listed set of roles is the exact defect being fixed: the pool
    that dispatched on the stale model lived two levels down, and the
    sidecar names its model `model.name` rather than `model`.
    """
    config = dict(CONFIG_SOL)
    config["sidecar"] = {"model": {"name": "gpt-5.6-luna"}}
    config["easy_worker"] = {"provider": "codex", "model": "gpt-5.6-terra",
                             "fallback_models": ["gpt-5.6-sol"]}
    path = Path(__file__).parent / "_tmp_cfg.json"
    try:
        path.write_text(json.dumps(config), encoding="utf-8")
        assert config_models(path) == {"gpt-5.6-sol", "gpt-5.6-luna", "gpt-5.6-terra"}
    finally:
        path.unlink(missing_ok=True)


def test_the_verifier_pool_bug_is_reported(tmp_path: Path) -> None:
    """The real shape: every wrong burst is role=reviewer, so only the
    per-category model split can distinguish them from the review lane."""
    ledger = make_run(tmp_path, CONFIG_SOL, [
        row("gpt-5.6-sol", "theorem_stating:worker:codex:gpt-5.6-sol:xhigh", role="worker"),
        row("gpt-5.6-sol", "theorem_stating:reviewer:review:codex:gpt-5.6-sol:xhigh"),
        row("gpt-5.5", "theorem_stating:reviewer:corr:1:v1:codex:gpt-5.5:xhigh"),
        row("gpt-5.5", "theorem_stating:reviewer:sound:2:v1:codex:gpt-5.5:xhigh"),
        row("gpt-5.5", "theorem_stating:reviewer:paper:3:v1:codex:gpt-5.5:xhigh"),
    ])
    code, out = run_mix(ledger)

    assert code == 0, "plain report must not exit nonzero; --strict is opt-in"
    assert out["ok"] is False
    flagged = {(b["category"], b["model"]) for b in out["unexpected"]}
    assert flagged == {("corr", "gpt-5.5"), ("sound", "gpt-5.5"), ("paper", "gpt-5.5")}

    # The two correctly-configured lanes must NOT be flagged — a check
    # that flags everything teaches operators to ignore it.
    ok_rows = {(b["category"], b["model"]) for b in out["by_category_model"] if b["expected"]}
    assert ("worker", "gpt-5.6-sol") in ok_rows
    assert ("review", "gpt-5.6-sol") in ok_rows

    # All five bursts share role=reviewer/worker only — proving the split
    # had to come from the scope's category, not the role.
    assert all(set(b["roles"]) <= {"worker", "reviewer"} for b in out["by_category_model"])


def test_clean_run_is_silent_and_strict_exits_zero(tmp_path: Path) -> None:
    ledger = make_run(tmp_path, CONFIG_SOL, [
        row("gpt-5.6-sol", "theorem_stating:worker:codex:gpt-5.6-sol:xhigh", role="worker"),
        row("gpt-5.6-sol", "theorem_stating:reviewer:corr:1:v1:codex:gpt-5.6-sol:xhigh"),
    ])
    code, out = run_mix(ledger, "--strict")
    assert out["unexpected"] == []
    assert out["ok"] is True
    assert code == 0


def test_strict_exits_nonzero_only_on_a_real_mismatch(tmp_path: Path) -> None:
    ledger = make_run(tmp_path, CONFIG_SOL, [
        row("gpt-5.5", "theorem_stating:reviewer:corr:1:v1:codex:gpt-5.5:xhigh"),
    ])
    code, _ = run_mix(ledger, "--strict")
    assert code == 1, "cron/CI form must fail when a lane runs an unconfigured model"


def test_unreadable_config_fails_open(tmp_path: Path) -> None:
    """No baseline => nothing to say. Never flag on ignorance.

    Published/exported run repos legitimately have no config at the
    expected path (verified against crossing-consequences-pub, 4196 rows).
    Treating that as "every model is unexpected" would make the check
    useless exactly where history is longest.
    """
    repo = tmp_path / "run"
    ledger = repo / ".trellis" / "logs" / "cost-ledger.jsonl"
    write_ledger(ledger, [row("gpt-5.5", "theorem_stating:reviewer:corr:1:v1:x:y:z")])
    code, out = run_mix(ledger, "--strict")
    assert out["config_readable"] is False
    assert out["unexpected"] == []
    assert out["ok"] is True
    assert code == 0


def test_recency_separates_live_drift_from_a_past_upgrade(tmp_path: Path) -> None:
    """`last_seen` is what makes the flag actionable.

    A finished run whose config was upgraded has every pre-upgrade row
    unexpected forever — true, but nothing to fix. Without a timestamp
    that is indistinguishable from a lane misconfigured right now.
    """
    old, new = 1_780_000_000.0, 1_787_900_000.0
    ledger = make_run(tmp_path, CONFIG_SOL, [
        row("gpt-5.4", "theorem_stating:reviewer:corr:1:v1:codex:gpt-5.4:xhigh", ts=old),
        row("gpt-5.5", "theorem_stating:reviewer:sound:2:v1:codex:gpt-5.5:xhigh", ts=new),
    ])
    _, out = run_mix(ledger)
    by_model = {b["model"]: b for b in out["unexpected"]}
    assert by_model["gpt-5.4"]["last_seen"] < by_model["gpt-5.5"]["last_seen"]
    assert by_model["gpt-5.4"]["last_seen"].startswith("2026-05")
    assert by_model["gpt-5.5"]["last_seen"].startswith("2026-08")


def test_unpinned_lane_records_what_the_cli_chose(tmp_path: Path) -> None:
    """An empty `model` means the provider CLI picked — which changes under
    CLI upgrades with nothing in the config moving. Surface it separately."""
    unpinned = {"role": "worker", "model": "", "scope": "theorem_stating:worker:codex::xhigh",
                "ts": 1_787_900_000.0, "cost_usd": 1.0, "usage": {"model": "gpt-5.6-sol"}}
    ledger = make_run(tmp_path, CONFIG_SOL, [unpinned])
    _, out = run_mix(ledger)
    assert out["unpinned_lane_defaults"], "a lane that pinned nothing must be visible"
    assert out["unpinned_lane_defaults"][0]["ran"] == "gpt-5.6-sol"
