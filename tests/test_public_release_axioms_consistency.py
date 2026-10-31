"""Audit M-2 regression — single-source-of-truth for canonical axioms.

The public-tablet-viewer export script
(`scripts/export_public_tablet_viewer.py`) carries a hard-coded
`DEFAULT_APPROVED_AXIOMS` list that must agree with the kernel's
`model.rs::CANONICAL_APPROVED_AXIOMS`. The Rust side has its own
regression test
(`kernel/src/runtime_cli_observations.rs::tests::default_approved_axioms_matches_canonical_constant`)
that pins the engine and runtime-CLI constants. This Python test pins
the Python copy by reading the Rust source directly — keeps the
regression bidirectional so a PR that updates one without the other
breaks CI loudly.
"""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MODEL_RS = ROOT / "kernel" / "src" / "model.rs"
EXPORT_SCRIPT = ROOT / "scripts" / "export_public_tablet_viewer.py"

# Extract the literal list from a const declaration in model.rs:
#
#   pub const CANONICAL_APPROVED_AXIOMS: &[&str] =
#       &["propext", "funext", "Classical.choice", "Quot.sound"];
#
# The regex is intentionally loose around whitespace / newlines because
# rustfmt may rearrange the line shape.
CANONICAL_RE = re.compile(
    r"pub\s+const\s+CANONICAL_APPROVED_AXIOMS\s*:\s*&\[&str\]\s*=\s*&\[(.*?)\];",
    re.DOTALL,
)


def _parse_rust_string_list(blob: str) -> list[str]:
    """Pull `"..."` items out of a Rust array literal body."""
    items = re.findall(r'"([^"]*)"', blob)
    return items


def _read_canonical_from_rust() -> list[str]:
    text = MODEL_RS.read_text(encoding="utf-8")
    match = CANONICAL_RE.search(text)
    if match is None:
        raise RuntimeError(
            "could not find CANONICAL_APPROVED_AXIOMS literal in kernel/src/model.rs; "
            "either the regex needs updating or the constant moved (audit M-2 single "
            "source of truth was disturbed without updating tests)"
        )
    return _parse_rust_string_list(match.group(1))


def _read_python_default() -> list[str]:
    # Import the constant by executing only the module's top-level
    # statements would require running the script's imports; safer to
    # parse the literal directly.
    text = EXPORT_SCRIPT.read_text(encoding="utf-8")
    match = re.search(
        r"DEFAULT_APPROVED_AXIOMS\s*=\s*\[(.*?)\]",
        text,
        re.DOTALL,
    )
    if match is None:
        raise RuntimeError(
            "could not find DEFAULT_APPROVED_AXIOMS literal in "
            "scripts/export_public_tablet_viewer.py; if the constant moved, "
            "update this test"
        )
    items = re.findall(r'"([^"]*)"', match.group(1))
    return items


def test_python_export_default_matches_rust_canonical() -> None:
    rust = _read_canonical_from_rust()
    python = _read_python_default()
    assert set(rust) == set(python), (
        "DEFAULT_APPROVED_AXIOMS in scripts/export_public_tablet_viewer.py "
        f"({sorted(python)}) does not match the kernel-wide canonical list "
        f"in kernel/src/model.rs ({sorted(rust)}). Update both in the same "
        "commit. Audit M-2 single source of truth."
    )


def test_canonical_contains_mathlib_blessed_four() -> None:
    """Defensive: the canonical four must always be present.

    Catches a PR that drops an axiom by accident (set-equality alone
    would let an empty list pass identically on both sides).
    """
    rust = _read_canonical_from_rust()
    for required in ["propext", "funext", "Classical.choice", "Quot.sound"]:
        assert required in rust, (
            f"canonical approved-axioms list lost required mathlib-blessed "
            f"axiom: {required}"
        )
