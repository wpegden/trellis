"""Lean-closure body contract: `-- BODY` split/splice and the body token ban.

Relocated from the retired closure sidecar's driver tests; the dispatch
candidate gate (trellis/dispatch/candidate.py) is the remaining consumer.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from trellis.dispatch.body_contract import (
    BANNED_TOKENS,
    SIDECAR_EXTRA_BANNED_TOKENS,
    body_ban_scan,
    splice_body,
    split_body_marker,
)

REPO_ROOT = Path(__file__).resolve().parents[1]

NODE_FILE = (
    "import Tablet.Preamble\nimport Tablet.Dep\n\n-- [TABLET NODE: Rung]\n"
    "theorem Rung : True := by\n-- BODY\n  sorry\n"
)


# ---------------------------------------------------------------------------
# Marker split + splice property (adversarial)
# ---------------------------------------------------------------------------


def test_split_requires_exactly_one_marker() -> None:
    prefix, body = split_body_marker(NODE_FILE)
    assert prefix.endswith("-- BODY\n")
    assert body == "  sorry\n"
    with pytest.raises(ValueError):
        split_body_marker(NODE_FILE + "-- BODY\n")
    with pytest.raises(ValueError):
        split_body_marker("theorem X : True := by\n  trivial\n")


@pytest.mark.parametrize(
    "adversarial_body",
    [
        "  trivial\n-- BODY\n  sorry\n",  # smuggled marker
        NODE_FILE,  # whole-file emission
        "  -- ∀ ε > 0 ∃ δ 🎯 unicode\r\n  trivial\r\n",  # unicode/CRLF
        "```lean\n  trivial\n```\n",  # markdown fence residue
        "",  # empty
    ],
)
def test_splice_never_mutates_the_prefix(adversarial_body: str) -> None:
    prefix, _ = split_body_marker(NODE_FILE)
    spliced = splice_body(prefix, adversarial_body)
    assert spliced[: len(prefix)] == prefix, "prefix must be byte-identical"
    assert spliced[len(prefix) :] == adversarial_body


# ---------------------------------------------------------------------------
# Ban scan (A3)
# ---------------------------------------------------------------------------


def test_ban_list_mirrors_kernel_constants() -> None:
    """BANNED_TOKENS = FORBIDDEN_KEYWORDS_DEFAULT (Rust-mirrored, sorryAx
    first) + SIDECAR_EXTRA_BANNED_TOKENS (mirror of the Rust
    kernel/src/sidecar.rs constant, A3 tokens included)."""
    assert BANNED_TOKENS[0] == "sorryAx"
    for a3_token in ("attribute", "deriving", "export"):
        assert a3_token in SIDECAR_EXTRA_BANNED_TOKENS
    sidecar_rs = (REPO_ROOT / "kernel" / "src" / "sidecar.rs").read_text()
    match = re.search(
        r"pub const SIDECAR_EXTRA_BANNED_TOKENS: &\[&str\] = &\[(.*?)\];",
        sidecar_rs,
        re.DOTALL,
    )
    assert match is not None
    rust_extras = re.findall(r'"([^"]+)"', match.group(1))
    assert list(SIDECAR_EXTRA_BANNED_TOKENS) == rust_extras


def test_ban_scan_hits_every_token_and_respects_boundaries() -> None:
    for token in BANNED_TOKENS:
        assert body_ban_scan(f"  exact foo\n  {token} bar\n") == token
    # Comments included.
    assert body_ban_scan("  trivial -- attribute [simp] hidden\n") == "attribute"
    # Identifier containment does not trip.
    assert body_ban_scan("  exact attributeFoo my_export' derivingX sorryish\n") is None
    assert body_ban_scan("  intro h\n  simpa using h\n") is None


def test_ban_scan_rejects_exec_command_family() -> None:
    """Finding 1: the elaboration-time arbitrary-IO exec primitives.
    `run_tac` is a TACTIC reachable directly in a proof body; `run_elab`
    / `run_meta` are command-slot forms. Each executes IO under the
    tablet toolchain and must be rejected; `run_cmd`/`#eval` were already
    covered."""
    for token in ("run_tac", "run_elab", "run_meta"):
        assert token in BANNED_TOKENS
    assert body_ban_scan("  run_tac do pure ()\n  trivial\n") == "run_tac"
    assert body_ban_scan("  trivial\nrun_elab do pure ()\n") == "run_elab"
    assert body_ban_scan("  trivial\nrun_meta do pure ()\n") == "run_meta"
    # Prefix identifiers do not trip the token-boundary scan.
    assert body_ban_scan("  exact run_tactical\n") is None
    assert body_ban_scan("  exact run_elaborate\n") is None
