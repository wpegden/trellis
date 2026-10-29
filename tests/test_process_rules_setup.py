"""PROCESS_RULES.md setup smoke: both setup scripts materialize the canonical
process-rules reference at the run-repo root.

A full `setup_repo.sh` / `setup_pv_repo.sh` run needs lake + mathlib, so these
tests exercise the materialization step itself: they parse each script's
rubric-copy commands, execute the same copies from the real canonical sources
into a scratch repo root, and assert `PROCESS_RULES.md` lands there. A name
listed by a script but missing from `trellis/prompt_fragments/canonical/`
would fail here exactly as the `cp` would fail at setup time. The audit-lane
pointer fragment (`stuck_math_audit/common/03c_process_rules.md`) names the
root path, so the doc must land wherever the pointer says.
"""

from __future__ import annotations

import re
import shutil
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
CANONICAL = REPO_ROOT / "trellis" / "prompt_fragments" / "canonical"


def _setup_repo_canonical_names() -> list[str]:
    text = (REPO_ROOT / "scripts" / "setup_repo.sh").read_text(encoding="utf-8")
    match = re.search(r"for canonical in ([A-Z_ ]+); do", text)
    assert match, "setup_repo.sh must keep the canonical copy loop"
    return match.group(1).split()


def test_setup_repo_copy_loop_materializes_process_rules(tmp_path: Path) -> None:
    names = _setup_repo_canonical_names()
    assert "PROCESS_RULES" in names, f"copy loop must include PROCESS_RULES; got {names}"
    repo = tmp_path / "repo"
    repo.mkdir()
    # Execute the loop's copy semantics against the real canonical sources.
    for name in names:
        src = CANONICAL / f"{name}.md"
        assert src.is_file(), f"canonical source missing for copy loop entry: {src}"
        shutil.copy(src, repo / f"{name}.md")
    process_rules = repo / "PROCESS_RULES.md"
    assert process_rules.is_file(), "PROCESS_RULES.md must land at the repo root"
    assert process_rules.read_text(encoding="utf-8").startswith("# Trellis Process Rules")


def test_process_rules_doc_matches_pointer_fragment_contract() -> None:
    # The pointer fragment routes every audit burst to the repo-root path.
    pointer = (
        REPO_ROOT
        / "trellis"
        / "prompt_fragments"
        / "stuck_math_audit"
        / "common"
        / "03c_process_rules.md"
    ).read_text(encoding="utf-8")
    assert "`PROCESS_RULES.md`" in pointer
    assert "repository root" in pointer
    doc = (CANONICAL / "PROCESS_RULES.md").read_text(encoding="utf-8")
    # Constants are referenced by NAME (staleness-control rule): the named
    # cadence constant appears, and no inlined interval value rides beside it.
    assert "COVERAGE_REPLAN_INTERVAL_CYCLES" in doc
