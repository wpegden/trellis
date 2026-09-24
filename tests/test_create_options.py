"""Drift guard: the create wizard's option lists must follow their sources.

`python3 -m trellis.create_options` is what the viewer's create-options.json
endpoint actually runs, so that subprocess — not an in-process call — is the
surface under test, and it is compared against the sources imported
directly: the pricing tables in trellis.agents.tmux_backend and the
KNOWN_EFFORTS registry in trellis.config. Same stance as the
kernel-agreement guard in test_resolve_paper_targets.py: two independent
paths to the same answer, held equal, so a source change the emission fails
to follow (a cached copy, a filter, a broken import) breaks HERE instead of
silently offering the browser a stale set.

The lists inform and never gate — validation stays a charset check — so the
one coupling that must hold is the reverse one: everything OFFERED must
pass the create flow's charset validation, or the suggestion itself would
4xx on submit. That check reads the regexes out of
scripts/trellis_create_run.sh (the script is validate_overrides' home;
viewer/server.js mirrors it) rather than restating them.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

from trellis.agents.tmux_backend import (
    _CLAUDE_PRICING_USD_PER_MTOK,
    _CODEX_PRICING_USD_PER_MTOK,
)
from trellis.config import KNOWN_EFFORTS

REPO_ROOT = Path(__file__).resolve().parents[1]


def emitted_options() -> dict:
    process = subprocess.run(
        [sys.executable, "-m", "trellis.create_options"],
        capture_output=True,
        text=True,
        timeout=60,
        cwd=REPO_ROOT,
    )
    assert process.returncode == 0, process.stderr
    return json.loads(process.stdout)


def test_model_options_follow_the_pricing_tables() -> None:
    """Per provider, the emitted model values ARE the pricing-table keys,
    in table order — no additions, no omissions, no third provider invented.
    """
    by_provider: dict[str, list[str]] = {}
    for row in emitted_options()["models"]:
        by_provider.setdefault(row["provider"], []).append(row["value"])
    assert by_provider.get("codex") == list(_CODEX_PRICING_USD_PER_MTOK)
    assert by_provider.get("claude") == list(_CLAUDE_PRICING_USD_PER_MTOK)
    # Gemini pricing is plan-based and has no table; a gemini (or any other)
    # model list here would have to be a hardcoded copy — the thing this
    # whole surface exists to avoid.
    assert set(by_provider) == {"codex", "claude"}


def test_effort_options_follow_the_registry() -> None:
    """The emitted (value, provider) pairs are exactly the registry's.

    Emission merges providers per value (one combo-list row per distinct
    tier), so equality is on the pair set plus registry-order preservation
    for first occurrences — not on a flattened list.
    """
    emitted = emitted_options()["efforts"]
    emitted_pairs = {
        (row["value"], provider) for row in emitted for provider in row["providers"]
    }
    registry_pairs = {
        (tier, provider) for provider, tiers in KNOWN_EFFORTS.items() for tier in tiers
    }
    assert emitted_pairs == registry_pairs

    first_seen: list[str] = []
    for _, tiers in KNOWN_EFFORTS.items():
        for tier in tiers:
            if tier not in first_seen:
                first_seen.append(tier)
    assert [row["value"] for row in emitted] == first_seen


def _script_charset(var: str) -> re.Pattern[str]:
    """The validate_overrides charset for MODEL_ARG / EFFORT_ARG, read out
    of the script source (the test_config.py TEX_STATEMENT_ENVS stance:
    pin against the source's own spelling, never a second copy)."""
    source = (REPO_ROOT / "scripts" / "trellis_create_run.sh").read_text(encoding="utf-8")
    match = re.search(rf'\[\[ "\${var}" =~ (\^\S+\$) \]\]', source)
    assert match, f"validate_overrides pattern for {var} not found in trellis_create_run.sh"
    return re.compile(match.group(1))


def test_every_offered_value_passes_create_flow_validation() -> None:
    """Offered ⊆ accepted, never the reverse: a suggestion the form's own
    validation would refuse converts an advisory list into a trap."""
    emitted = emitted_options()
    model_re = _script_charset("MODEL_ARG")
    effort_re = _script_charset("EFFORT_ARG")
    for row in emitted["models"]:
        assert model_re.match(row["value"]), row
    for row in emitted["efforts"]:
        assert effort_re.match(row["value"]), row


def test_browser_appends_derived_rows_and_keeps_freeform_escape_hatches() -> None:
    """The new selects preserve the old advisory-list contract.

    Pinned models lead, but the pricing/registry rows still reach every
    dropdown and ``other…`` still reaches a text input. This is the design
    tension's option (b), held at the client seam where it could regress to a
    closed three-item select without changing the JSON producer.
    """
    landing = (REPO_ROOT / "viewer" / "public" / "landing.html").read_text(
        encoding="utf-8"
    )
    assert "createChoiceRows(pinnedModels, (wizard.options && wizard.options.models)" in landing
    assert "createChoiceRows([defaultEffort], (wizard.options && wizard.options.efforts)" in landing
    assert "el('option', null, 'other…')" in landing
    assert "custom.type = 'text'" in landing
    assert "<datalist" not in landing
