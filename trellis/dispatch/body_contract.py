"""Lean-closure write contract: the `-- BODY` split and the body token ban.

The kernel's filespec splitter and `sidecar::SIDECAR_EXTRA_BANNED_TOKENS`
are the authorities; these pure-text mirrors let the dispatch candidate
gate enforce the same contract host-side before anything reaches the
kernel. The Rust constant is pinned by tests/test_body_contract.py.
"""
from __future__ import annotations

from typing import Optional, Tuple

from trellis.config import FORBIDDEN_KEYWORDS_DEFAULT

# Mirror of kernel `sidecar::SIDECAR_EXTRA_BANNED_TOKENS`
# (kernel/src/sidecar.rs): tokens banned in sidecar bodies BEYOND
# `LEAN_FORBIDDEN_KEYWORDS`. `attribute` / `deriving` / `export` are
# amendment A3 — command forms that slip past the declaration-shape
# gate and can globally mutate typeclass / reducibility / name
# resolution for OTHER nodes. Order matters for first-hit reporting;
# keep in lockstep with the Rust constant (pinned by test).
SIDECAR_EXTRA_BANNED_TOKENS: Tuple[str, ...] = (
    "admit",
    "macro_rules",
    "elab_rules",
    "declare_syntax_cat",
    "binder_predicate",
    "attribute",
    "deriving",
    "export",
    # Anonymous `initialize (...)` declares no name, so the gate-6b delta
    # cannot see it; it executes at IMPORT inside the compile sandbox.
    # Only a ban reaches a nameless command. 0/626 live bodies contain it.
    "initialize",
    "builtin_initialize",
)

BANNED_TOKENS: Tuple[str, ...] = tuple(FORBIDDEN_KEYWORDS_DEFAULT) + (
    SIDECAR_EXTRA_BANNED_TOKENS
)

BODY_MARKER = "-- BODY"



# ---------------------------------------------------------------------------
# FILESPEC split (pure text scan — the kernel splitter's rule)
# ---------------------------------------------------------------------------


def split_body_marker(content: str) -> Tuple[str, str]:
    """Return ``(prefix_through_marker_line, body)``. Exactly one line
    whose trimmed content is ``-- BODY`` is required (the kernel
    ``filespec_split::split`` rule)."""
    offset = 0
    found: Optional[Tuple[int, int]] = None
    for line in content.splitlines(keepends=True):
        end = offset + len(line)
        if line.rstrip("\r\n").strip() == BODY_MARKER:
            if found is not None:
                raise ValueError("multiple `-- BODY` marker lines")
            found = (offset, end)
        offset = end
    if found is None:
        raise ValueError("no `-- BODY` marker line")
    _, marker_end = found
    return content[:marker_end], content[marker_end:]


def splice_body(prefix: str, body: str) -> str:
    """The one write shape the driver ever produces."""
    return prefix + body


# ---------------------------------------------------------------------------
# Ban scan (pre-compile; mirrors the kernel-side sidecar gate)
# ---------------------------------------------------------------------------

_TOKEN_CHARS = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_'")


def _contains_token(text: str, token: str) -> bool:
    start = 0
    while True:
        idx = text.find(token, start)
        if idx < 0:
            return False
        before = text[idx - 1] if idx > 0 else ""
        after_idx = idx + len(token)
        after = text[after_idx] if after_idx < len(text) else ""
        if before not in _TOKEN_CHARS and after not in _TOKEN_CHARS:
            return True
        start = after_idx


def body_ban_scan(body: str) -> Optional[str]:
    """First banned token in the body (comments/strings INCLUDED —
    deliberately stricter than the primary masked scan), or None."""
    for token in BANNED_TOKENS:
        if _contains_token(body, token):
            return token
    return None
