"""Campaign-independent checks of the prose GOAL parser on a real example."""

import re
from pathlib import Path

import pytest

from scripts.pv_prose_goal import parse_goal


ROOT = Path(__file__).resolve().parents[1]
EXAMPLE = next((ROOT / "examples").glob("pv_*/source/PROVENANCE.md")).parent.parent
GOAL = EXAMPLE / "prose" / "GOAL.md"
RAW_SOURCE_ROOT = EXAMPLE / "source"


def test_example_goal_headings_select_public_raw_source_functions() -> None:
    """A real GOAL exercises the same heading-to-source contract as any launch.

    Targets bind to Aeneas-emitted item provenance, which covers crate-visible
    functions (`pub` or `pub(crate)`) in any file of the source tree.
    """
    targets = parse_goal(GOAL)["targets"]
    source = "\n".join(
        path.read_text(encoding="utf-8")
        for path in sorted(RAW_SOURCE_ROOT.rglob("*.rs"))
    )

    assert targets
    assert all(target["resolution"] == "decide" for target in targets)
    assert all(
        re.search(rf"\bpub(?:\([a-z]+\))?\s+fn\s+{re.escape(target['name'])}\s*[<(]", source)
        for target in targets
    )


def test_exact_resolution_markers_and_default(tmp_path: Path) -> None:
    goal = tmp_path / "GOAL.md"
    goal.write_text(
        "# Goal\n\n### `defaulted`\n\n### `explicit_decide` [decide]\n\n"
        "### `positive_only` [prove]\n",
        encoding="utf-8",
    )
    assert parse_goal(goal)["targets"] == [
        {"name": "defaulted", "resolution": "decide", "line": 3},
        {"name": "explicit_decide", "resolution": "decide", "line": 5},
        {"name": "positive_only", "resolution": "prove", "line": 7},
    ]


def test_unknown_or_inexact_resolution_marker_is_rejected(tmp_path: Path) -> None:
    headings = [
        "### `target` [conditional]",
        "### `target` [Prove]",
        "### `target`[prove]",
    ]
    for index, heading in enumerate(headings):
        goal = tmp_path / f"GOAL-{index}.md"
        goal.write_text(f"# Goal\n\n{heading}\n", encoding="utf-8")
        with pytest.raises(SystemExit, match="malformed target heading"):
            parse_goal(goal)
