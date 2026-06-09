#!/usr/bin/env python3
"""Rewrite a paper's \\newtheorem aliases to kernel-recognized env names.

The trellis kernel (kernel/src/paper_targets.rs) extracts paper statement
blocks by matching a hardcoded set of LaTeX env names:

    TEX_STATEMENT_ENVS      = {theorem, lemma, definition, corollary, proposition, helper}
    DEFAULT_MAIN_RESULT_ENVS = {theorem, corollary}

Many real papers define short aliases in the preamble, e.g.

    \\newtheorem{theo}{Theorem}[section]
    \\newtheorem{prop}[theo]{Proposition}
    \\newtheorem{cor}[theo]{Corollary}
    \\newtheorem{defn}[theo]{Definition}

and then write \\begin{theo}, \\begin{cor}, \\begin{defn}, ... The kernel
silently skips these because the env names don't match its list, which makes
main-result resolution fail and verifier paper-statement extraction incomplete.

This script reads the paper preamble, detects alias-to-canonical mappings from
each \\newtheorem declaration (matching the title case-insensitively against
the canonical env set), and rewrites:

    \\begin{<alias>}            -> \\begin{<canonical>}
    \\end{<alias>}              -> \\end{<canonical>}
    \\newtheorem{<alias>}{T}    -> \\newtheorem{<canonical>}{T}
    \\newtheorem{X}[<alias>]{T} -> \\newtheorem{X}[<canonical>]{T}

A paper that already uses canonical env names is passed through unchanged.

Usage:
    normalize_paper_envs.py <input.tex> <output.tex>
        Write a normalized copy to <output.tex>. Prints rename stats to stderr.

    normalize_paper_envs.py --check <input.tex>
        Print detected alias map and exit. No file writes.
"""
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

CANONICAL_ENVS: set[str] = {
    "theorem",
    "lemma",
    "definition",
    "corollary",
    "proposition",
    "helper",
}

# Matches \newtheorem[*]{<alias>}[<counter_pre>?]{<title>}[<counter_post>?].
# Both counter positions are valid LaTeX (counter is optional, and may come
# either before or after the title; we tolerate either order).
NEWTHM_RE = re.compile(
    r"\\newtheorem\*?\s*"
    r"\{(?P<alias>[^}]+)\}\s*"
    r"(?:\[(?P<counter_pre>[^\]]+)\]\s*)?"
    r"\{(?P<title>[^}]+)\}"
    r"(?:\s*\[(?P<counter_post>[^\]]+)\])?"
)


def build_alias_map(text: str) -> dict[str, str]:
    """Scan \\newtheorem declarations and return alias -> canonical env name.

    A declaration contributes a mapping only when its title (case-insensitive)
    is one of the kernel's canonical envs AND the alias differs from the
    canonical name. Declarations like ``\\newtheorem{rem}{Remark}`` produce
    nothing (Remark isn't canonical), which is the right behavior: the kernel
    doesn't extract remarks regardless.
    """
    aliases: dict[str, str] = {}
    for match in NEWTHM_RE.finditer(text):
        alias = match.group("alias").strip()
        title = match.group("title").strip().lower()
        if title not in CANONICAL_ENVS:
            continue
        if alias == title:
            continue
        existing = aliases.get(alias)
        if existing is not None and existing != title:
            print(
                f"warning: alias {alias!r} maps to both {existing!r} and {title!r}; "
                f"keeping first ({existing!r})",
                file=sys.stderr,
            )
            continue
        aliases[alias] = title
    return aliases


def rewrite_paper(text: str, alias_map: dict[str, str]) -> tuple[str, dict[str, int]]:
    """Apply ``alias_map`` to begin/end markers and \\newtheorem declarations."""
    stats: dict[str, int] = {}
    if not alias_map:
        return text, stats

    for alias, canonical in alias_map.items():
        for marker in ("begin", "end"):
            text, count = re.subn(
                r"\\" + marker + r"\{" + re.escape(alias) + r"\}",
                "\\\\" + marker + "{" + canonical + "}",
                text,
            )
            if count:
                stats[f"{marker}:{alias}->{canonical}"] = count

    def rewrite_newthm(match: re.Match[str]) -> str:
        alias = match.group("alias").strip()
        counter_pre_raw = match.group("counter_pre")
        title = match.group("title")
        counter_post_raw = match.group("counter_post")

        new_alias = alias_map.get(alias, alias)
        if new_alias != alias:
            stats[f"newthm-alias:{alias}->{new_alias}"] = (
                stats.get(f"newthm-alias:{alias}->{new_alias}", 0) + 1
            )

        def maybe_rename_counter(raw: str | None) -> str | None:
            if raw is None:
                return None
            stripped = raw.strip()
            renamed = alias_map.get(stripped, stripped)
            if renamed != stripped:
                stats[f"newthm-counter:{stripped}->{renamed}"] = (
                    stats.get(f"newthm-counter:{stripped}->{renamed}", 0) + 1
                )
            return renamed

        new_counter_pre = maybe_rename_counter(counter_pre_raw)
        new_counter_post = maybe_rename_counter(counter_post_raw)

        out = "\\newtheorem{" + new_alias + "}"
        if new_counter_pre is not None:
            out += "[" + new_counter_pre + "]"
        out += "{" + title + "}"
        if new_counter_post is not None:
            out += "[" + new_counter_post + "]"
        return out

    text = NEWTHM_RE.sub(rewrite_newthm, text)
    return text, stats


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Normalize LaTeX \\newtheorem aliases to kernel-canonical env names.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("input", type=Path, help="Source paper .tex")
    parser.add_argument(
        "output",
        type=Path,
        nargs="?",
        help="Destination .tex (omit with --check)",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="Print detected alias map and exit without writing.",
    )
    args = parser.parse_args(argv)

    text = args.input.read_text(encoding="utf-8")
    aliases = build_alias_map(text)

    if args.check:
        if not aliases:
            print(
                f"{args.input}: no normalizable aliases detected (paper uses canonical env names)",
                file=sys.stderr,
            )
            return 0
        print(f"{args.input}: detected aliases:", file=sys.stderr)
        for alias, canonical in sorted(aliases.items()):
            print(f"  {alias!r} -> {canonical!r}", file=sys.stderr)
        return 0

    if args.output is None:
        parser.error("output path is required (use --check to inspect aliases only)")

    new_text, stats = rewrite_paper(text, aliases)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(new_text, encoding="utf-8")

    if not aliases:
        print(
            f"{args.input}: passed through unchanged (no normalizable aliases)",
            file=sys.stderr,
        )
        return 0

    print(f"{args.input}: normalized to {args.output}", file=sys.stderr)
    pretty_map = ", ".join(f"{a}->{c}" for a, c in sorted(aliases.items()))
    print(f"  alias map: {pretty_map}", file=sys.stderr)
    for key in sorted(stats):
        print(f"  {key}: {stats[key]}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
