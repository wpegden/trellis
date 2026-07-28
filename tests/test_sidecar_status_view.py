"""Sidecar grunt-status table renderer (queue redesign §3.2, A6).

Advisory fresh-read surface: queue rows ALWAYS inline in full;
eligible_now rows inline never-attempted-first, then prior-attempt
history, up to the line limit (default 40, env-overridable); paths +
limit injected for the fragment. Absent / corrupt files degrade to
sentinels, never raise.
"""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path

from trellis.runtime.bridge_prompts import (
    SIDECAR_STATUS_PROMPT_MAX_LINES_DEFAULT,
    SIDECAR_STATUS_PROMPT_MAX_LINES_ENV,
    sidecar_status_block,
)


def _seed(tmp_path: Path, export: dict, status: dict | None = None) -> Path:
    """Seed the two files the block reads.

    A status dict with no ``generated_at_ms`` gets a FRESH one: these
    fixtures stand for a daemon that is up, and a file with no timestamp
    is by definition stale, which would put every assertion below into
    the daemon-is-down rendering."""
    sidecar = tmp_path / "sidecar"
    sidecar.mkdir(parents=True, exist_ok=True)
    (sidecar / "candidates.json").write_text(json.dumps(export))
    if status is not None:
        status = dict(status)
        status.setdefault("generated_at_ms", int(time.time() * 1000))
        (sidecar / "status.json").write_text(json.dumps(status))
    return tmp_path


def _fake_proc(tmp_path: Path, pid: int, argv: list[str]) -> Path:
    """A synthetic ``/proc`` tree (the injectable seam ``adopt`` uses)
    with one process in it."""
    proc_root = tmp_path / "proc"
    entry = proc_root / str(pid)
    entry.mkdir(parents=True, exist_ok=True)
    (entry / "cmdline").write_bytes(("\0".join(argv) + "\0").encode("utf-8"))
    return proc_root


def _daemon_argv(runtime_root: Path) -> list[str]:
    """The production daemon cmdline (``scripts/trellis_sidecar.sh``)."""
    return [
        sys.executable,
        "-m",
        "trellis.sidecar",
        str(runtime_root),
        "--repo",
        str(runtime_root / "repo"),
    ]


def _attempt_argv(runtime_root: Path) -> list[str]:
    """An attempt CHILD's cmdline — the argv that must never be read as
    the manager's."""
    return [
        sys.executable,
        "-m",
        "trellis.sidecar.attempt",
        str(runtime_root),
        "--grunt",
        "0",
        "--node",
        "A",
        "--entry-seq",
        "1",
        "--attempt-id",
        "sc-x",
    ]


def test_absent_surface_returns_sentinel_and_paths(tmp_path: Path) -> None:
    block, cand, stat, limit = sidecar_status_block(tmp_path)
    assert "no sidecar runtime surface" in block
    assert cand.endswith("sidecar/candidates.json")
    assert stat.endswith("sidecar/status.json")
    assert limit == str(SIDECAR_STATUS_PROMPT_MAX_LINES_DEFAULT)


def test_queue_rows_always_inline_in_full(tmp_path: Path) -> None:
    n = SIDECAR_STATUS_PROMPT_MAX_LINES_DEFAULT + 20
    export = {
        "schema": 2,
        "cycle": 12,
        "sidecar_window_open": True,
        "queue": [
            {"node": f"Q{i}", "entry_seq": i + 1, "queued_at_cycle": 3,
             "status": "ready"}
            for i in range(n)
        ],
        "eligible_now": [],
        "pruned_recent": [],
    }
    _seed(tmp_path, export)
    block, _, _, _ = sidecar_status_block(tmp_path)
    for i in range(n):
        assert f"- Q{i} [seq {i + 1}" in block, f"queue row Q{i} must be inline"
    assert "omitted" not in block


def test_eligible_rows_capped_with_omission_note(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setenv(SIDECAR_STATUS_PROMPT_MAX_LINES_ENV, "5")
    export = {
        "schema": 2,
        "cycle": 12,
        "sidecar_window_open": True,
        "queue": [],
        # Kernel export order is tier-then-name; the renderer keeps
        # export order, so tier-1 rows survive the head truncation.
        "eligible_now": (
            [{"node": f"T1n{i}", "tier": 1} for i in range(5)]
            + [{"node": f"T2n{i}", "tier": 2} for i in range(10)]
        ),
        "pruned_recent": [],
    }
    _seed(tmp_path, export)
    block, _, _, limit = sidecar_status_block(tmp_path)
    assert limit == "5"
    for i in range(5):
        assert f"T1n{i} (tier 1)" in block
    assert "T2n0" not in block
    assert "10 more omitted (line cap 5)" in block


def test_in_flight_and_attempt_digests_render(tmp_path: Path) -> None:
    export = {
        "schema": 2,
        "cycle": 208,
        "sidecar_window_open": True,
        "queue": [
            {"node": "Rung", "entry_seq": 7, "queued_at_cycle": 200,
             "status": "ready"},
            {"node": "Held", "entry_seq": 8, "queued_at_cycle": 201,
             "status": "blocked:held_target"},
        ],
        "eligible_now": [{"node": "Lift", "tier": 2}],
        "pruned_recent": [
            {"node": "Old", "entry_seq": 3, "cycle": 207, "reason": "closed"}
        ],
    }
    status = {
        "grunts": 2,
        "in_flight": [
            {"node": "Rung", "entry_seq": 7, "grunt": 0, "attempt_id": "sc-x"}
        ],
        "attempts": {
            "Lift": [
                {"entry_seq": 5, "status": "failed",
                 "detail": "compile: unsolved goals", "ts": 1},
                {"entry_seq": 6, "status": "failed",
                 "detail": "wall budget", "ts": 2},
            ]
        },
    }
    _seed(tmp_path, export, status)
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "Sidecar window: open (export cycle 208)" in block
    assert "IN FLIGHT on grunt 0 (attempt sc-x)" in block
    assert "blocked:held_target" in block
    assert "Lift (tier 2) — attempted x2, last: failed (wall budget)" in block
    assert "Old [seq 3] pruned: closed (cycle 207)" in block
    # Failure digests still render (regression guard against the closures
    # addition).
    assert "attempted x2, last: failed" in block


def test_recent_closures_render_newest_first_with_attribution(tmp_path: Path) -> None:
    export = {
        "schema": 2,
        "cycle": 300,
        "sidecar_window_open": True,
        "queue": [],
        "eligible_now": [],
        "pruned_recent": [],
        # Kernel emits newest-first; the renderer keeps export order.
        "recent_closures": [
            {"node": "Beta", "cycle": 295, "model": "codex"},
            {"node": "Alpha", "cycle": 290, "model": "mistral"},
        ],
    }
    _seed(tmp_path, export)
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert (
        "Recent grunt closures (2): Beta (cyc 295, codex), Alpha (cyc 290, mistral)"
        in block
    )


def test_recent_closures_empty_renders_none(tmp_path: Path) -> None:
    export = {
        "schema": 2,
        "cycle": 5,
        "sidecar_window_open": True,
        "queue": [],
        "eligible_now": [],
        "pruned_recent": [],
    }
    _seed(tmp_path, export)
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "Recent grunt closures: (none)" in block


def test_recent_closures_capped_at_line_budget(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setenv(SIDECAR_STATUS_PROMPT_MAX_LINES_ENV, "3")
    export = {
        "schema": 2,
        "cycle": 300,
        "sidecar_window_open": True,
        "queue": [],
        "eligible_now": [],
        "pruned_recent": [],
        "recent_closures": [
            {"node": f"N{i}", "cycle": 300 - i, "model": "m"} for i in range(8)
        ],
    }
    _seed(tmp_path, export)
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "Recent grunt closures (8): " in block
    assert "N0 (cyc 300, m)" in block
    assert "N2 (cyc 298, m)" in block
    assert "N3" not in block
    assert "... 5 more omitted (line cap 3)" in block


def test_corrupt_files_degrade_without_raising(tmp_path: Path) -> None:
    sidecar = tmp_path / "sidecar"
    sidecar.mkdir(parents=True)
    (sidecar / "candidates.json").write_text("{not json")
    (sidecar / "status.json").write_text("]also broken")
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "Sidecar window: closed" in block
    assert "Queue: (empty" in block


def test_none_runtime_root_is_safe() -> None:
    block, cand, stat, limit = sidecar_status_block(None)
    assert "no sidecar runtime surface" in block
    assert "<runtime>" in cand and "<runtime>" in stat
    assert limit == str(SIDECAR_STATUS_PROMPT_MAX_LINES_DEFAULT)


# --- pool-occupancy / spent-generation visibility (idle-starvation guard) ---
#
# Live regression (perfect run, cycles 332-342): the queue held three
# entries whose generations the daemon had already spent, both grunts
# were idle for over an hour with 190 eligible nodes, and the block the
# reviewer received said only "Queue (3 entries, reviewer order)" with
# each row's kernel status "ready". Nothing named the pool size, the
# free-slot count, or the fact that a spent entry is not future work.


def _spent_queue_export(cycle: int = 342) -> dict:
    """The live c342 shape: three entries, each already attempted once
    on its current generation."""
    return {
        "schema": 2,
        "cycle": cycle,
        "sidecar_window_open": True,
        "queue": [
            {"node": "F6Complement", "entry_seq": 8, "queued_at_cycle": 319,
             "status": "ready"},
            {"node": "DoubleDiamondFormsCube", "entry_seq": 9,
             "queued_at_cycle": 320, "status": "ready"},
        ],
        "eligible_now": [{"node": "PerfectBerge", "tier": 1}],
        "pruned_recent": [],
    }


def _spent_status() -> dict:
    return {
        "grunts": 2,
        "in_flight": [],
        "attempts": {
            "F6Complement": [
                {"entry_seq": 8, "status": "budget_exhausted",
                 "detail": "wall budget", "ts": 1}
            ],
            "DoubleDiamondFormsCube": [
                {"entry_seq": 9, "status": "budget_exhausted",
                 "detail": "wall budget", "ts": 2}
            ],
        },
    }


def test_pool_occupancy_line_names_size_and_free_slots(tmp_path: Path) -> None:
    _seed(tmp_path, _spent_queue_export(), _spent_status())
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "Grunt pool: 2 worker(s), 0 attempt(s) in flight, 2 idle" in block


def test_in_flight_attempts_count_against_the_pool(tmp_path: Path) -> None:
    export = _spent_queue_export()
    status = _spent_status()
    status["in_flight"] = [
        {"node": "F6Complement", "entry_seq": 8, "grunt": 1, "attempt_id": "sc-y"}
    ]
    _seed(tmp_path, export, status)
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "Grunt pool: 2 worker(s), 1 attempt(s) in flight, 1 idle" in block


def test_spent_generation_is_marked_on_the_queue_row(tmp_path: Path) -> None:
    _seed(tmp_path, _spent_queue_export(), _spent_status())
    block, _, _, _ = sidecar_status_block(tmp_path)
    for line in block.splitlines():
        if line.startswith("- F6Complement [seq 8"):
            assert "; SPENT;" in line
            # The failure digest still rides alongside the marker.
            assert "attempted x1, last: budget_exhausted (wall budget)" in line
            break
    else:  # pragma: no cover - the row must exist
        raise AssertionError(f"F6Complement queue row missing:\n{block}")


def test_fully_spent_queue_states_the_pool_is_idle(tmp_path: Path) -> None:
    _seed(tmp_path, _spent_queue_export(), _spent_status())
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert (
        "Queue holds no assignable entry, so 2 grunt(s) sit idle: "
        "every entry is spent, in flight, or blocked." in block
    )


def test_fresh_generation_after_re_add_is_not_spent(tmp_path: Path) -> None:
    """A remove + re-add mints a new entry_seq: the OLD attempt row must
    not mark the new generation spent, or the reviewer would be told a
    live entry is dead."""
    export = _spent_queue_export()
    export["queue"] = [
        {"node": "F6Complement", "entry_seq": 14, "queued_at_cycle": 345,
         "status": "ready"}
    ]
    _seed(tmp_path, export, _spent_status())
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "SPENT" not in block
    assert "sit idle" not in block
    # The prior generation's failure stays visible as history.
    assert "attempted x1, last: budget_exhausted (wall budget)" in block


def test_assignable_entry_suppresses_the_idle_note(tmp_path: Path) -> None:
    export = _spent_queue_export()
    export["queue"].append(
        {"node": "PerfectComplement", "entry_seq": 13, "queued_at_cycle": 344,
         "status": "ready"}
    )
    _seed(tmp_path, export, _spent_status())
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "sit idle" not in block


def test_blocked_entry_does_not_count_as_assignable(tmp_path: Path) -> None:
    export = _spent_queue_export()
    export["queue"].append(
        {"node": "PerfectComplement", "entry_seq": 13, "queued_at_cycle": 344,
         "status": "blocked:active_node"}
    )
    _seed(tmp_path, export, _spent_status())
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "sit idle" in block


def test_missing_daemon_status_degrades_without_a_pool_claim(tmp_path: Path) -> None:
    _seed(tmp_path, _spent_queue_export())
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "Grunt pool: (size unknown — no daemon status file)" in block
    assert "SPENT" not in block
    assert "sit idle" not in block


def test_spent_legend_renders_once_not_per_row(tmp_path: Path) -> None:
    _seed(tmp_path, _spent_queue_export(), _spent_status())
    block, _, _, _ = sidecar_status_block(tmp_path)
    legend = [
        line for line in block.splitlines() if line.startswith("SPENT = ")
    ]
    assert len(legend) == 1, f"expected exactly one legend line, got {legend}"
    assert "remove now plus an add on a later cycle" in legend[0]
    assert "both in one response is illegal" in legend[0]


def test_no_spent_entry_means_no_legend(tmp_path: Path) -> None:
    export = _spent_queue_export()
    export["queue"] = [
        {"node": "PerfectComplement", "entry_seq": 13, "queued_at_cycle": 344,
         "status": "ready"}
    ]
    _seed(tmp_path, export, _spent_status())
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "SPENT" not in block


# ---------------------------------------------------------------------------
# Spent-generation retirement — the history the auto-prune must not hide
# ---------------------------------------------------------------------------


def test_auto_pruned_node_failure_is_still_visible(tmp_path: Path) -> None:
    """The auto-prune removes a queue ENTRY, not the history behind it.

    A node whose generation the kernel just retired leaves the queue
    immediately, so the queue block (the reviewer's densest surface)
    stops mentioning it. It must therefore show up in BOTH remaining
    places: the retired-generation prune section carrying its digest,
    and — because it is eligible again — the add feed carrying the same
    digest. Without this the reviewer re-queues a node whose failure
    they never saw."""
    export = {
        "schema": 2,
        "cycle": 208,
        "sidecar_window_open": True,
        "queue": [],
        "eligible_now": [{"node": "Lift", "tier": 2}],
        "pruned_recent": [
            {"node": "Lift", "entry_seq": 6, "cycle": 208,
             "reason": "attempt_spent:failed"},
            {"node": "Old", "entry_seq": 3, "cycle": 207, "reason": "closed"},
        ],
    }
    status = {
        "grunts": 2,
        "in_flight": [],
        "attempts": {
            "Lift": [
                {"entry_seq": 6, "status": "failed",
                 "detail": "compile: unsolved goals", "ts": 2},
            ]
        },
    }
    _seed(tmp_path, export, status)
    block, _, _, _ = sidecar_status_block(tmp_path)
    # Retirement and lifecycle prunes get separate headers: they mean
    # opposite things to the reviewer.
    assert "Retired spent generations" in block
    assert (
        "- Lift [seq 6] pruned: attempt_spent:failed (cycle 208) — "
        "attempted x1, last: failed (compile: unsolved goals)" in block
    )
    assert "Recent kernel prunes (newest last):" in block
    assert "- Old [seq 3] pruned: closed (cycle 207)" in block
    # And the add feed still carries the digest inline.
    assert "Lift (tier 2) — attempted x1, last: failed" in block
    assert "Prior grunt attempts: 1 eligible node(s)" in block


def test_never_attempted_rows_lead_the_eligible_feed(tmp_path: Path) -> None:
    """The feed is the reviewer's list of nodes it can ADD, so the rows
    it can act on lead it.

    History-carrying rows used to render first; they are still grouped
    together, but second — a node the pool has already burned a
    generation on is not a fresh add. The pin is against the kernel's
    own order: `Alpha0` sorts first there AND carries history, so
    seeing it after `Alpha1` is only possible if the grouping ran."""
    export = {
        "schema": 2,
        "cycle": 208,
        "sidecar_window_open": True,
        "queue": [],
        "eligible_now": [{"node": f"Alpha{i}", "tier": 1} for i in range(4)],
        "pruned_recent": [],
    }
    status = {
        "grunts": 2,
        "in_flight": [],
        "attempts": {
            "Alpha0": [{"entry_seq": 6, "status": "failed", "detail": "no", "ts": 2}]
        },
    }
    _seed(tmp_path, export, status)
    block, _, _, _ = sidecar_status_block(tmp_path)
    lines = block.splitlines()
    # Nothing is dropped here, so the digest is still inline — ranking
    # second is not hiding it.
    assert "- Alpha0 (tier 1) — attempted x1, last: failed (no)" in lines
    for i in (1, 2, 3):
        assert lines.index(f"- Alpha{i} (tier 1)") < lines.index(
            "- Alpha0 (tier 1) — attempted x1, last: failed (no)"
        )
    # And the header names the order it actually renders.
    assert (
        "Eligible now (4 add candidates, never-attempted first, then "
        "prior-attempt history, tier 1 first within each):" in lines
    )


def test_a_tried_population_over_the_cap_still_shows_actionable_adds(
    tmp_path: Path, monkeypatch
) -> None:
    """The failure this ordering exists to prevent.

    On `perfect` at cycle 478 the eligible feed carried 51 tried nodes
    against a 40-line cap, so history-first ordering left not one of the
    74 never-attempted nodes in the prompt: every name the reviewer
    could see had already spent its attempt, and it submitted a
    remove-only decision with both grunt slots idle. Whatever else the
    cap cuts, the actionable adds are what survives it."""
    monkeypatch.setenv(SIDECAR_STATUS_PROMPT_MAX_LINES_ENV, "3")
    export = {
        "schema": 2,
        "cycle": 478,
        "sidecar_window_open": True,
        # Kernel order is tier-then-name; every tried row sorts ahead of
        # every fresh one, so pure export order buries the fresh set.
        "eligible_now": (
            [{"node": f"Alpha{i}", "tier": 1} for i in range(5)]
            + [{"node": f"Zulu{i}", "tier": 1} for i in range(4)]
        ),
        "pruned_recent": [],
    }
    status = {
        "grunts": 2,
        "in_flight": [],
        "attempts": {
            f"Alpha{i}": [
                {"entry_seq": 6, "status": "failed", "detail": "no", "ts": 2}
            ]
            for i in range(5)
        },
    }
    _seed(tmp_path, export, status)
    block, cand, stat, _ = sidecar_status_block(tmp_path)
    rendered = [line for line in block.splitlines() if line.startswith("- ")]
    assert rendered == [
        "- Zulu0 (tier 1)",
        "- Zulu1 (tier 1)",
        "- Zulu2 (tier 1)",
    ]
    # The tried rows are gone from the feed, and both surfaces say so.
    assert "- Alpha0 (tier 1)" not in block
    assert (
        "Prior grunt attempts: 5 eligible node(s) have been tried before; the line "
        f"cap is spent before them, so none are shown below — all 5 are only in "
        f"{cand}." in block
    )
    assert "6 more omitted (line cap 3), 5 of them carrying prior-attempt history" in block
    assert stat in block


def test_prior_attempt_summary_absent_when_no_history(tmp_path: Path) -> None:
    export = {
        "schema": 2,
        "cycle": 208,
        "sidecar_window_open": True,
        "queue": [],
        "eligible_now": [{"node": "Lift", "tier": 2}],
        "pruned_recent": [],
    }
    _seed(tmp_path, export, {"grunts": 2, "in_flight": [], "attempts": {}})
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "Prior grunt attempts:" not in block


def test_spent_marker_legend_flags_a_broken_outcome_pipeline(tmp_path: Path) -> None:
    """The SPENT marker now covers only the window between a spend and
    the next boundary's ingest, so a SPENT row that persists is a direct
    signal that the outcome pipeline is broken. The legend says so."""
    export = {
        "schema": 2,
        "cycle": 208,
        "sidecar_window_open": True,
        "queue": [
            {"node": "Rung", "entry_seq": 7, "queued_at_cycle": 200,
             "status": "ready"}
        ],
        "eligible_now": [],
        "pruned_recent": [],
    }
    status = {
        "grunts": 2,
        "in_flight": [],
        "attempts": {
            "Rung": [{"entry_seq": 7, "status": "failed", "detail": "x", "ts": 1}]
        },
    }
    _seed(tmp_path, export, status)
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "; SPENT" in block
    assert "the outcome pipeline is broken" in block


def test_history_rows_the_cap_cuts_are_counted_not_claimed_shown(
    tmp_path: Path, monkeypatch
) -> None:
    """The hard case, and the one that grows into existence.

    Auto-prune pushes every retired node back into `eligible_now`
    CARRYING its history, and nothing but a rewind clears the
    attempted-set — so the history-carrying population only grows, and
    the never-attempted rows ahead of it leave it less and less of the
    budget. The block must not claim a history it dropped: it states how
    many were omitted, that they carry history, and where to read
    them."""
    monkeypatch.setenv(SIDECAR_STATUS_PROMPT_MAX_LINES_ENV, "4")
    export = {
        "schema": 2,
        "cycle": 208,
        "sidecar_window_open": True,
        "queue": [],
        "eligible_now": (
            [{"node": f"Tried{i}", "tier": 1} for i in range(5)]
            + [{"node": f"Fresh{i}", "tier": 1} for i in range(2)]
        ),
        "pruned_recent": [],
    }
    status = {
        "grunts": 2,
        "in_flight": [],
        "attempts": {
            f"Tried{i}": [
                {"entry_seq": 6, "status": "failed", "detail": "no", "ts": 2}
            ]
            for i in range(5)
        },
    }
    _seed(tmp_path, export, status)
    block, cand, stat, _ = sidecar_status_block(tmp_path)

    # Both fresh rows, then what the budget still holds of the history.
    rendered = [line for line in block.splitlines() if line.startswith("- ")]
    assert len(rendered) == 4
    assert rendered[:2] == ["- Fresh0 (tier 1)", "- Fresh1 (tier 1)"]
    assert all(line.startswith("- Tried") for line in rendered[2:])
    # The claim is accurate on both surfaces.
    assert (
        "Never attempted: 2 eligible node(s) have no prior grunt attempt "
        "(all inline below)."
    ) in block
    assert (
        "Prior grunt attempts: 5 eligible node(s) have been tried before; "
        f"the 2 shown below are what the line cap leaves, 3 more are only in {cand}."
    ) in block
    assert "3 more omitted (line cap 4), 3 of them carrying prior-attempt history" in block
    assert stat in block
    # And it must NOT assert the falsehood.
    assert "every row with prior-attempt history is shown above" not in block


def test_never_attempted_rows_the_cap_cuts_are_counted_too(
    tmp_path: Path, monkeypatch
) -> None:
    """The never-attempted group leads, so it is the one the cap cuts
    first once it alone outgrows the budget. Its summary line carries
    the same contract the history line does: how many are inline, how
    many are only in candidates.json."""
    monkeypatch.setenv(SIDECAR_STATUS_PROMPT_MAX_LINES_ENV, "3")
    export = {
        "schema": 2,
        "cycle": 208,
        "sidecar_window_open": True,
        "queue": [],
        "eligible_now": [{"node": f"Fresh{i}", "tier": 1} for i in range(7)],
        "pruned_recent": [],
    }
    _seed(tmp_path, export, {"grunts": 2, "in_flight": [], "attempts": {}})
    block, cand, _, _ = sidecar_status_block(tmp_path)
    assert (
        "Never attempted: 7 eligible node(s) have no prior grunt attempt; the 3 "
        f"shown below are the line cap's worth, 4 more are only in {cand}."
    ) in block
    assert "Prior grunt attempts:" not in block


def test_never_attempted_summary_absent_when_every_eligible_node_was_tried(
    tmp_path: Path,
) -> None:
    export = {
        "schema": 2,
        "cycle": 208,
        "sidecar_window_open": True,
        "queue": [],
        "eligible_now": [{"node": "Lift", "tier": 2}],
        "pruned_recent": [],
    }
    status = {
        "grunts": 2,
        "in_flight": [],
        "attempts": {
            "Lift": [{"entry_seq": 6, "status": "failed", "detail": "no", "ts": 2}]
        },
    }
    _seed(tmp_path, export, status)
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "Never attempted:" not in block


def test_history_rows_exactly_filling_the_rest_of_the_cap_are_all_shown(
    tmp_path: Path, monkeypatch
) -> None:
    """Boundary: history rows that fit in what the never-attempted rows
    leave are all shown, so the true claim is the one rendered."""
    monkeypatch.setenv(SIDECAR_STATUS_PROMPT_MAX_LINES_ENV, "3")
    export = {
        "schema": 2,
        "cycle": 208,
        "sidecar_window_open": True,
        "queue": [],
        "eligible_now": (
            [{"node": f"Tried{i}", "tier": 1} for i in range(2)]
            + [{"node": "Fresh0", "tier": 1}]
        ),
        "pruned_recent": [],
    }
    status = {
        "grunts": 2,
        "in_flight": [],
        "attempts": {
            f"Tried{i}": [{"entry_seq": 6, "status": "failed", "detail": "no", "ts": 2}]
            for i in range(2)
        },
    }
    _seed(tmp_path, export, status)
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert (
        "Never attempted: 1 eligible node(s) have no prior grunt attempt "
        "(all inline below)." in block
    )
    assert "Prior grunt attempts: 2 eligible node(s) have been tried before (history inline below)." in block
    assert "omitted" not in block


# ---------------------------------------------------------------------------
# Daemon liveness gating
#
# Live regression: the daemon died and every present-tense claim in this
# block kept rendering off the frozen status.json — "2 grunt(s) sit
# idle" (there was no pool at all) and "a SPENT row that persists means
# the outcome pipeline is broken" (it persists because the daemon that
# retires it is gone). The reviewer had no way to tell a healthy idle
# pool from an absent one, because they write the same file.
# ---------------------------------------------------------------------------


def _live_daemon(tmp_path: Path, pid: int = 9101) -> tuple[dict, Path]:
    """A status stamp + synthetic /proc for a RUNNING manager."""
    return (
        {"pid": pid, "generated_at_ms": int(time.time() * 1000)},
        _fake_proc(tmp_path, pid, _daemon_argv(tmp_path)),
    )


def test_running_daemon_renders_the_live_pool_claims(tmp_path: Path) -> None:
    stamp, proc_root = _live_daemon(tmp_path)
    _seed(tmp_path, _spent_queue_export(), {**_spent_status(), **stamp})
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "Sidecar daemon: RUNNING (pid 9101)" in block
    assert "Grunt pool: 2 worker(s), 0 attempt(s) in flight, 2 idle" in block
    assert "sit idle" in block
    assert "the outcome pipeline is broken" in block
    assert "not assessable" not in block


def test_dead_daemon_suppresses_every_live_pool_claim(tmp_path: Path) -> None:
    """The whole point: same file, no daemon behind it."""
    stamp, _ = _live_daemon(tmp_path)
    # An empty /proc: the recorded pid is running nothing.
    proc_root = tmp_path / "empty-proc"
    proc_root.mkdir()
    _seed(tmp_path, _spent_queue_export(), {**_spent_status(), **stamp})
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "Sidecar daemon: NOT RUNNING (pid 9101)" in block
    assert "Grunt pool: last known 2 worker(s), 0 attempt(s) in flight" in block
    assert "sit idle" not in block, "no idle CAPACITY claim without a daemon"
    assert (
        "Queue capacity: not assessable while the daemon's status is stale; "
        "queue entries remain valid." in block
    )
    # The SPENT legend keeps its meaning and drops its accusation.
    assert "; SPENT" in block
    assert "the outcome pipeline is broken" not in block
    assert "that is not evidence of a fault" in block


def test_an_attempt_child_is_never_read_as_the_manager(tmp_path: Path) -> None:
    """`trellis.sidecar.attempt` starts with `trellis.sidecar`. A prefix
    match here is the 15-minute-outage bug: a dead manager whose orphans
    outlived it would read UP."""
    pid = 3131
    proc_root = _fake_proc(tmp_path, pid, _attempt_argv(tmp_path))
    stamp = {"pid": pid, "generated_at_ms": int(time.time() * 1000)}
    _seed(tmp_path, _spent_queue_export(), {**_spent_status(), **stamp})
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "Sidecar daemon: NOT RUNNING" in block


def test_a_daemon_over_another_runtime_root_is_not_this_one(tmp_path: Path) -> None:
    pid = 3232
    other = tmp_path / "other-runtime"
    other.mkdir()
    proc_root = _fake_proc(tmp_path, pid, _daemon_argv(other))
    stamp = {"pid": pid, "generated_at_ms": int(time.time() * 1000)}
    _seed(tmp_path, _spent_queue_export(), {**_spent_status(), **stamp})
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "Sidecar daemon: NOT RUNNING" in block


def test_live_daemon_with_a_frozen_status_file_states_the_gap(tmp_path: Path) -> None:
    """Alive but wedged: the process is there, the file is not moving."""
    pid = 9101
    proc_root = _fake_proc(tmp_path, pid, _daemon_argv(tmp_path))
    stamp = {
        "pid": pid,
        "poll_seconds": 30,
        "generated_at_ms": int((time.time() - 4000) * 1000),
    }
    _seed(tmp_path, _spent_queue_export(), {**_spent_status(), **stamp})
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "Sidecar daemon: RUNNING (pid 9101), but there has been no daemon " in block
    assert "status update for 4000 s" in block
    # The raw fact, not a verdict: "degraded" belongs to the CLI exit code.
    assert "degraded" not in block
    assert "Grunt pool: last known" in block
    assert "sit idle" not in block


def test_suspended_daemon_is_not_reported_as_spare_capacity(tmp_path: Path) -> None:
    stamp, proc_root = _live_daemon(tmp_path)
    status = {
        **_spent_status(),
        **stamp,
        "suspended_until_ms": int((time.time() + 1500) * 1000),
        "last_pass": {"status": "suspended", "at_ms": stamp["generated_at_ms"]},
    }
    _seed(tmp_path, _spent_queue_export(), status)
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "Grunt pool is SUSPENDED for another" in block
    assert "Idle grunts here are not spare capacity." in block
    assert "sit idle" not in block


def test_journal_error_daemon_names_why_it_assigns_nothing(tmp_path: Path) -> None:
    """A `journal_error` daemon reaps and cancels forever and assigns
    nothing, with one log line to say so. Before `last_pass` it was
    indistinguishable from a healthy idle pool."""
    stamp, proc_root = _live_daemon(tmp_path)
    status = {
        **_spent_status(),
        **stamp,
        "last_pass": {"status": "journal_error", "at_ms": stamp["generated_at_ms"]},
    }
    _seed(tmp_path, _spent_queue_export(), status)
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "Grunt pool is assigning nothing (last pass: journal_error)" in block
    assert "cannot write sidecar/slots.json" in block
    assert "sit idle" not in block


def test_expired_suspension_does_not_suppress_the_idle_claim(tmp_path: Path) -> None:
    stamp, proc_root = _live_daemon(tmp_path)
    status = {
        **_spent_status(),
        **stamp,
        "suspended_until_ms": int((time.time() - 60) * 1000),
        "last_pass": {"status": "idle", "at_ms": stamp["generated_at_ms"]},
    }
    _seed(tmp_path, _spent_queue_export(), status)
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "SUSPENDED" not in block
    assert "sit idle" in block


def test_missing_status_file_says_the_daemon_state_is_unknown(tmp_path: Path) -> None:
    _seed(tmp_path, _spent_queue_export())
    block, _, _, _ = sidecar_status_block(tmp_path)
    assert "Sidecar daemon: UNKNOWN" in block
    assert "nothing below describes the pool" in block


def test_the_block_never_creates_or_locks_the_pid_file(tmp_path: Path) -> None:
    """This runs on the critical path of every review burst. It must not
    CREATE sidecar/daemon.pid (that invents state) and must not flock it
    (a momentary shared lock makes a daemon starting at that instant
    fail acquire_pid_lock with "already running")."""
    stamp, proc_root = _live_daemon(tmp_path)
    _seed(tmp_path, _spent_queue_export(), {**_spent_status(), **stamp})
    sidecar_status_block(tmp_path, proc_root=proc_root)
    assert not (tmp_path / "sidecar" / "daemon.pid").exists()

    # And with the file present-and-locked by a "daemon", the block
    # still renders rather than blocking on the lock.
    import fcntl
    import os as _os

    pid_path = tmp_path / "sidecar" / "daemon.pid"
    fd = _os.open(str(pid_path), _os.O_RDWR | _os.O_CREAT, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
        assert "Sidecar daemon: RUNNING (pid 9101)" in block
    finally:
        _os.close(fd)


def test_the_unknown_state_says_which_check_was_missing(tmp_path: Path) -> None:
    """`unknown` has two causes and they need different words: a status
    file with NO pid is a daemon older than this schema, while a pid we
    cannot resolve is a /proc we could not read. Printing "no pid in
    status.json" for the second states something false about a pid that
    is sitting right there in the file."""
    fresh = int(time.time() * 1000)
    proc_root = tmp_path / "absent-proc"  # not a directory at all

    # (a) no pid field: the freshness fallback, named honestly.
    _seed(tmp_path, _spent_queue_export(), {**_spent_status(), "generated_at_ms": fresh})
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "no pid in status.json; inferred from its freshness" in block

    # (b) a pid that /proc cannot answer for. The pid is real data and
    # must not be described as missing.
    _seed(
        tmp_path,
        _spent_queue_export(),
        {**_spent_status(), "pid": 4242, "generated_at_ms": fresh},
    )
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "no pid in status.json" not in block
    assert "pid 4242, but /proc could not be read for it" in block

    # (c) a malformed pid is neither of those.
    _seed(
        tmp_path,
        _spent_queue_export(),
        {**_spent_status(), "pid": "nope", "generated_at_ms": fresh},
    )
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "not a usable process id" in block


def test_a_stale_unknown_names_its_reason_too(tmp_path: Path) -> None:
    _seed(
        tmp_path,
        _spent_queue_export(),
        {
            **_spent_status(),
            "pid": 4242,
            "generated_at_ms": int((time.time() - 5000) * 1000),
        },
    )
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=tmp_path / "absent-proc")
    assert "liveness UNDETERMINED (pid 4242, but /proc could not be read for it)" in block
    assert "Grunt pool: last known" in block


def test_awaiting_ingest_pool_is_not_reported_as_idle(tmp_path: Path) -> None:
    """AUDIT P5. A pool holding every ready entry back because its
    closure is published and waiting on the kernel used to report
    `idle` — which is not in `_SIDECAR_NOT_ASSIGNING`, so the block
    showed idle grunts alongside ready entries. The likely response to
    that reading is remove-and-re-add, which DISCARDS the finished
    closure and mints a fresh generation. The reason must be named, and
    the block must say not to touch those entries."""
    stamp, proc_root = _live_daemon(tmp_path)
    status = {
        **_spent_status(),
        **stamp,
        "last_pass": {
            "status": "awaiting_ingest",
            "at_ms": stamp["generated_at_ms"],
            "awaiting_ingest": ["Alpha", "Beta"],
        },
    }
    _seed(tmp_path, _spent_queue_export(), status)
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "Grunt pool is assigning nothing (last pass: awaiting_ingest)" in block
    assert "waiting for the kernel to ingest it" in block
    assert "Idle grunts here are not spare capacity." in block
    # The nodes are named, and the destructive response is called out.
    assert "awaiting kernel ingest: Alpha, Beta" in block
    assert "DISCARD" in block
    assert "sit idle" not in block


def test_awaiting_ingest_node_list_is_capped(tmp_path: Path) -> None:
    """The named list rides inside a line-capped prompt block, so it is
    truncated rather than allowed to crowd the queue rows out."""
    stamp, proc_root = _live_daemon(tmp_path)
    status = {
        **_spent_status(),
        **stamp,
        "last_pass": {
            "status": "awaiting_ingest",
            "at_ms": stamp["generated_at_ms"],
            "awaiting_ingest": [f"N{i}" for i in range(12)],
        },
    }
    _seed(tmp_path, _spent_queue_export(), status)
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "awaiting kernel ingest: N0, N1, N2, N3, N4, N5, N6, N7 (+4 more)" in block


def test_awaiting_ingest_without_a_node_list_still_renders(tmp_path: Path) -> None:
    """A daemon from before this version writes the status but no node
    list; the block degrades to the reason line."""
    stamp, proc_root = _live_daemon(tmp_path)
    status = {
        **_spent_status(),
        **stamp,
        "last_pass": {"status": "awaiting_ingest", "at_ms": stamp["generated_at_ms"]},
    }
    _seed(tmp_path, _spent_queue_export(), status)
    block, _, _, _ = sidecar_status_block(tmp_path, proc_root=proc_root)
    assert "Grunt pool is assigning nothing (last pass: awaiting_ingest)" in block
    assert "awaiting kernel ingest:" not in block
