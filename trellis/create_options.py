"""Derived model/effort suggestion lists for the create wizard.

The viewer's ``create-options.json`` endpoint runs ``python3 -m
trellis.create_options`` and relays the JSON this prints. Both lists are
derived at call time from the one place the codebase already answers the
question — never copied:

- **models**: the keys of the provider pricing tables in
  ``trellis.agents.tmux_backend`` (``_CODEX_PRICING_USD_PER_MTOK``,
  ``_CLAUDE_PRICING_USD_PER_MTOK``). A CLI update that adds a model to a
  pricing table reaches the browser through this import with no viewer
  edit. Note the honesty limit, surfaced in ``sources``: a pricing table
  is the set the system can COST, not the set it can RUN — a model can be
  usable but unpriced (type it) or priced but retired.
- **efforts**: ``trellis.config.KNOWN_EFFORTS``, the per-provider advisory
  registry that lives next to the ``effort`` field it describes.

ADVISORY ONLY. These lists are appended after the launcher's pinned choices
in ``<select>`` controls whose final ``other…`` item reveals a freeform
input. They must never become a validation allowlist: the create flow's
checks (``validatedOverrides`` in viewer/server.js,
``validate_overrides`` in scripts/trellis_create_run.sh) stay charset
checks precisely so an unlisted-but-real value still starts a run.
tests/test_create_options.py pins this output to the sources above, so a
source change the emission fails to follow breaks a test instead of
silently offering a stale set.
"""

from __future__ import annotations

import json
from typing import Any, Dict, List

from trellis.agents.tmux_backend import (
    _CLAUDE_PRICING_USD_PER_MTOK,
    _CODEX_PRICING_USD_PER_MTOK,
)
from trellis.config import KNOWN_EFFORTS

# Table order is the pricing-table authors' arrangement; provider order puts
# codex first because the shipped templates run codex on every lane.
_MODEL_TABLES = (
    ("codex", _CODEX_PRICING_USD_PER_MTOK),
    ("claude", _CLAUDE_PRICING_USD_PER_MTOK),
)


def _price(value: Any) -> str:
    return f"${float(value):g}"


def model_options() -> List[Dict[str, Any]]:
    """One row per pricing-table key: value, provider, display label.

    The label carries what the source actually knows about each model —
    its provider and its input/output rates — so the page can render it
    verbatim without recomputing anything.
    """
    rows: List[Dict[str, Any]] = []
    for provider, table in _MODEL_TABLES:
        for name, pricing in table.items():
            rows.append({
                "value": str(name),
                "provider": provider,
                "label": (
                    f"{provider} · {_price(pricing['input'])}/"
                    f"{_price(pricing['output'])} per Mtok in/out"
                ),
            })
    return rows


def effort_options() -> List[Dict[str, Any]]:
    """One row per distinct effort value, providers merged.

    Distinct because tiers recur across providers ("high" is both a codex
    and a claude tier) and a combo list showing the same value twice reads
    as a bug. Registry order is preserved; a value keeps its first
    position and accumulates providers.
    """
    by_value: Dict[str, Dict[str, Any]] = {}
    for provider, tiers in KNOWN_EFFORTS.items():
        for tier in tiers:
            row = by_value.setdefault(str(tier), {"value": str(tier), "providers": []})
            if provider not in row["providers"]:
                row["providers"].append(provider)
    rows = list(by_value.values())
    for row in rows:
        row["label"] = ", ".join(row["providers"])
    return rows


def create_options() -> Dict[str, Any]:
    return {
        "options_version": 1,
        "models": model_options(),
        "efforts": effort_options(),
        "sources": {
            "models": (
                "pricing-table keys in trellis/agents/tmux_backend.py — the set "
                "the system can COST; a usable-but-unpriced model is legitimate, "
                "type it"
            ),
            "efforts": "trellis.config.KNOWN_EFFORTS (advisory registry)",
        },
    }


def main() -> None:
    print(json.dumps(create_options(), indent=2))


if __name__ == "__main__":
    main()
