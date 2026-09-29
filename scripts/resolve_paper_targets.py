#!/usr/bin/env python3
"""Resolve a paper's main-result candidates and explain every block that missed.

This is the diagnostic target resolver behind the viewer's target-selection
page (VIEWER_RUN_CREATION_DESIGN.md §5.1-§5.4). It emits one JSON document:

  * ``candidates``      - the blocks the kernel's resolver accepts, in document
                          order, with the advisory ranking of §5.4 layered on
                          top (``rank`` / ``rank_reasons`` / ``preselected``);
  * ``rejected_blocks`` - every theorem-like block that did NOT become a
                          candidate, each with a reason code and an
                          author-facing fix-it (§5.2);
  * ``envs_present``    - every environment the paper actually uses or declares
                          via ``\\newtheorem``, with counts and suggested alias
                          mappings, feeding the environments control (§5.3);
  * ``file_warnings`` / ``scan_truncated`` - file-level findings, including the
                          two cases §1.3 declares undiagnosable.

Authority split: the kernel (kernel/src/paper_targets.rs) remains the only
authority on what *is* a candidate. The scan here mirrors
``extract_statement_blocks_with_envs`` exactly - same document window, same
comment stripping, same case-sensitive ``\\end`` termination, same dedup - so
that the diagnostics describe the real resolver rather than an idealized one.
Pass ``--kernel-cmd`` to have the mirror checked against the kernel itself: a
disagreement is reported as a ``mirror_disagreement`` file warning instead of
being silently papered over.

The ranking is presentation only. It never changes what the kernel accepts,
and the headless setup path (no page) keeps kernel inference untouched.

Usage:
    resolve_paper_targets.py paper.tex
    resolve_paper_targets.py paper.tex --out targets_resolution.json
    resolve_paper_targets.py paper.tex --main-result-envs theorem,corollary,proposition
    resolve_paper_targets.py paper.tex --env-map thm=theorem --env-map cor=corollary
    resolve_paper_targets.py paper.tex --kernel-cmd kernel/target/debug/trellis_runtime_cli
"""
from __future__ import annotations

import argparse
import bisect
import hashlib
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any, Iterable

sys.path.insert(0, str(Path(__file__).resolve().parent))

from normalize_paper_envs import (  # noqa: E402  (path set above)
    CANONICAL_ENVS,
    NEWTHM_RE,
    build_alias_map,
    rewrite_paper,
)

# Mirrors kernel/src/paper_targets.rs.
TEX_STATEMENT_ENVS = sorted(CANONICAL_ENVS)
DEFAULT_MAIN_RESULT_ENVS = ["theorem", "corollary"]

BEGIN_MARKER = "\\begin{"
END_MARKER = "\\end{"
DOC_BEGIN = "\\begin{document}"
DOC_END = "\\end{document}"

# Undeclared abbreviations common enough that silence would be less honest than
# a "this env yields no candidates" note. Only the first three letters are used
# to *suggest* a canonical target; the reason code states a kernel fact either
# way (an env outside TEX_STATEMENT_ENVS is never scanned).
ALIAS_HINTS = {
    "thm",
    "thms",
    "theo",
    "thrm",
    "cor",
    "coro",
    "corol",
    "lem",
    "lemm",
    "prop",
    "propn",
    "defn",
    "conj",
    "clm",
    "claim",
}
CANONICAL_BY_PREFIX = {env[:3]: env for env in TEX_STATEMENT_ENVS}

REF_RE = re.compile(r"\\[A-Za-z]*ref\*?\s*\{([^}]*)\}")
SECTION_RE = re.compile(r"\\(?:sub)*section\*?\s*[\[{]")
MAIN_RESULT_RE = re.compile(
    r"main\s+(?:result|theorem)|principal\s+result|our\s+main\b", re.IGNORECASE
)
INPUT_RE = re.compile(r"\\(input|include)\s*\{([^}]*)\}")
LEADING_LABEL_RE = re.compile(r"\A\s*\\label\s*\{([^}]*)\}")
MALFORMED_ENV_RE = re.compile(r"[\s\\]")
# `\begin {theorem}` compiles fine in LaTeX; the kernel matches the literal
# string `\begin{`, so the whole block is invisible to it.
SPACED_BEGIN_RE = re.compile(r"\\begin[ \t]+\{([^}\n]*)\}")
SPACED_END_RE = re.compile(r"\\end[ \t]+\{([^}\n]*)\}")

RANK_PRESELECT_THRESHOLD = 2.0
LONG_BLOCK_CHARS = 300


# ---------------------------------------------------------------------------
# Kernel mirror: text preparation
# ---------------------------------------------------------------------------

def _split_inclusive_lf(text: str) -> list[str]:
    """Rust's ``split_inclusive('\\n')``: keep the newline, drop the empty tail."""
    parts = text.split("\n")
    out = [part + "\n" for part in parts[:-1]]
    if parts[-1]:
        out.append(parts[-1])
    return out


def _comment_cut(line: str) -> int:
    """Index of the first unescaped ``%`` in ``line`` (or ``len(line)``)."""
    cut = len(line)
    backslashes = 0
    for index, char in enumerate(line):
        if char == "\\":
            backslashes += 1
            continue
        if char == "%":
            if backslashes % 2 == 0:
                cut = index
                break
            backslashes = 0
            continue
        backslashes = 0
    return cut


def strip_with_offsets(text: str, base: int = 0) -> tuple[str, list[int]]:
    """Comment-strip ``text`` and return the surviving chars' absolute offsets."""
    chunks: list[str] = []
    offsets: list[int] = []
    cursor = 0
    for segment in _split_inclusive_lf(text):
        if segment.endswith("\n"):
            line, newline = segment[:-1], "\n"
        else:
            line, newline = segment, ""
        cut = _comment_cut(line)
        chunks.append(line[:cut])
        offsets.extend(range(base + cursor, base + cursor + cut))
        if newline:
            chunks.append(newline)
            offsets.append(base + cursor + len(line))
        cursor += len(segment)
    return "".join(chunks), offsets


def mask_comments(text: str) -> str:
    """``text`` with commented-out characters replaced by NUL (offsets intact)."""
    out: list[str] = []
    for segment in _split_inclusive_lf(text):
        if segment.endswith("\n"):
            line, newline = segment[:-1], "\n"
        else:
            line, newline = segment, ""
        cut = _comment_cut(line)
        out.append(line[:cut])
        out.append("\0" * (len(line) - cut))
        out.append(newline)
    return "".join(out)


def document_window(raw: str) -> tuple[int, int, bool]:
    """Kernel R6: first ``\\begin{document}`` / ``\\end{document}`` on raw text."""
    begin = raw.find(DOC_BEGIN)
    end = raw.find(DOC_END)
    if begin != -1 and end != -1:
        begin_end = begin + len(DOC_BEGIN)
        if end >= begin_end:
            return begin_end, end, True
    return 0, len(raw), False


class LineIndex:
    def __init__(self, text: str) -> None:
        self._newlines = [i for i, char in enumerate(text) if char == "\n"]

    def line_of(self, offset: int) -> int:
        return bisect.bisect_left(self._newlines, offset) + 1


# ---------------------------------------------------------------------------
# Kernel mirror: block scan
# ---------------------------------------------------------------------------

class Block:
    __slots__ = (
        "env",
        "raw_env",
        "title",
        "text",
        "labels",
        "start_line",
        "end_line",
        "begin_off",
        "end_off",
    )

    def __init__(self, **kwargs: Any) -> None:
        for key, value in kwargs.items():
            setattr(self, key, value)


def extract_labels(block_text: str) -> list[str]:
    """Mirror of the kernel's ``extract_labels`` (no regex, first-``}`` bound)."""
    labels: list[str] = []
    seen: set[str] = set()
    search = 0
    marker = "\\label{"
    while True:
        offset = block_text.find(marker, search)
        if offset == -1:
            break
        start = offset + len(marker)
        end = block_text.find("}", start)
        if end == -1:
            break
        label = block_text[start:end].strip()
        if label and label not in seen:
            seen.add(label)
            labels.append(label)
        search = end + 1
    return labels


def env_name_at(text: str, open_at: int) -> str | None:
    """Mirror of the kernel's ``env_name_at``: the bounded brace-group name.

    A name is name characters (plus the padding the ``\\begin`` side tolerates)
    terminated by ``}`` before the line ends. A braceless ``\\end{`` — quoted
    LaTeX in a proof — is not a name, and must not consume the document up to
    some distant ``}`` that belongs to a real closing marker.
    """
    for index in range(open_at, len(text)):
        char = text[index]
        if char == "}":
            return text[open_at:index]
        if not (char.isascii() and (char.isalnum() or char in "*@_- \t")):
            return None
    return None


def walk_block_end(
    text: str, start: int, raw_env: str, env: str
) -> tuple[str, int, int, str]:
    """Mirror of the kernel's ``find_block_end``, keeping why the walk stopped.

    Returns ``("closed", begin, stop, name)`` for the block's own ``\\end``,
    ``("mismatch", begin, stop, name)`` for an ``\\end`` naming this
    environment in a spelling that closes nothing open (invalid LaTeX: the walk
    stops there and absorbs nothing beyond it), or ``("open", -1, -1, "")``.

    ``Theorem`` and ``theorem`` are different LaTeX environments, so a ``\\end``
    in the other spelling may legitimately close a nested block; those are
    balanced out. A ``\\begin`` spelled exactly like ours is not tracked —
    same-spelling nesting truncates the outer block at the first ``\\end``,
    which is the kernel's long-standing flat rule.
    """
    nested: list[str] = []
    search = start
    while True:
        next_begin = text.find(BEGIN_MARKER, search)
        next_end = text.find(END_MARKER, search)
        if next_begin == -1 and next_end == -1:
            return "open", -1, -1, ""
        if next_begin != -1 and (next_end == -1 or next_begin < next_end):
            at, is_begin, marker = next_begin, True, BEGIN_MARKER
        else:
            at, is_begin, marker = next_end, False, END_MARKER
        open_at = at + len(marker)
        name = env_name_at(text, open_at)
        if name is None:
            search = open_at
            continue
        brace = open_at + len(name)
        search = brace + 1
        name = name.strip()
        if name.lower() != env:
            continue
        if is_begin:
            if name != raw_env:
                nested.append(name)
            continue
        if name == raw_env:
            return "closed", at, brace + 1, name
        if nested and nested[-1] == name:
            nested.pop()
            continue
        return "mismatch", at, brace + 1, name


def find_block_end(text: str, start: int, raw_env: str, env: str) -> tuple[int, int] | None:
    """Where the block opened as ``raw_env`` closes, or None."""
    kind, begin, stop, _ = walk_block_end(text, start, raw_env, env)
    return (begin, stop) if kind == "closed" else None


def scan_blocks(
    stripped: str,
    offsets: list[int],
    lines: LineIndex,
    envs: set[str],
) -> tuple[list[Block], int | None]:
    """Mirror of ``extract_statement_blocks_with_envs``.

    Returns the blocks plus the raw offset where a malformed ``\\begin{``
    abandoned the scan (kernel: ``break``), or ``None``.
    """
    blocks: list[Block] = []
    index = 0
    truncated_at: int | None = None
    while True:
        begin = stripped.find(BEGIN_MARKER, index)
        if begin == -1:
            break
        env_start = begin + len(BEGIN_MARKER)
        env_end = stripped.find("}", env_start)
        if env_end == -1:
            truncated_at = offsets[begin]
            break
        raw_env = stripped[env_start:env_end].strip()
        env = raw_env.lower()
        index = env_end + 1
        if env not in envs:
            continue
        content_start = env_end + 1
        title = ""
        if stripped[content_start:content_start + 1] == "[":
            title_end = stripped.find("]", content_start + 1)
            if title_end != -1:
                title = stripped[content_start + 1:title_end].strip()
                content_start = title_end + 1
        closes = find_block_end(stripped, content_start, raw_env, env)
        if closes is None:
            continue
        _, end_end = closes
        full_block = stripped[begin:end_end].strip()
        blocks.append(
            Block(
                env=env,
                raw_env=raw_env,
                title=title,
                text=full_block,
                labels=extract_labels(full_block),
                start_line=lines.line_of(offsets[begin]),
                end_line=lines.line_of(offsets[end_end - 1]),
                begin_off=offsets[begin],
                end_off=offsets[end_end - 1] + 1,
            )
        )
        index = end_end
    return blocks, truncated_at


def target_key(label: str | None, start_line: int, end_line: int) -> str:
    """Page-facing key. Mirrors the kernel's identity, minus the ``label:`` tag."""
    if label:
        return label
    return f"lines:{start_line}-{end_line}"


def direct_labels(block_text: str) -> set[str]:
    """Labels declared by the block itself, not by an environment nested in it.

    Diagnostics only: the kernel deliberately does not care (``extract_labels``
    is a flat scan), but the author does. ``\\label{eq:1}`` inside a nested
    ``equation`` was never a candidate name for the theorem, so it must not draw
    a "put this label first" fix-it.
    """
    labels: set[str] = set()
    depth = 0
    index = block_text.find("}")  # skip the block's own \begin{env}
    if index == -1:
        return labels
    while index < len(block_text):
        nxt = min(
            (pos for pos in (
                block_text.find(BEGIN_MARKER, index),
                block_text.find("\\end{", index),
                block_text.find("\\label{", index),
            ) if pos != -1),
            default=-1,
        )
        if nxt == -1:
            break
        if block_text.startswith(BEGIN_MARKER, nxt):
            depth += 1
            index = nxt + len(BEGIN_MARKER)
        elif block_text.startswith("\\end{", nxt):
            depth -= 1
            index = nxt + len("\\end{")
        else:
            start = nxt + len("\\label{")
            end = block_text.find("}", start)
            if end == -1:
                break
            if depth == 0:
                label = block_text[start:end].strip()
                if label:
                    labels.add(label)
            index = end + 1
    return labels


def dedup_blocks(
    blocks: list[Block],
) -> tuple[list[Block], list[tuple[Block, Block, str]]]:
    """Kernel ``infer_main_result_targets_from_blocks``: first block with a key wins.

    Dropped blocks carry the *kind* of key that collided: a shared label is a
    different author problem from two unlabeled blocks that happen to share a
    line range, and they need different advice.
    """
    kept: list[Block] = []
    dropped: list[tuple[Block, Block, str]] = []
    seen: dict[str, Block] = {}
    for block in blocks:
        label = block.labels[0] if block.labels else None
        key = ("label:" + label) if label else f"lines:{block.start_line}-{block.end_line}"
        winner = seen.get(key)
        if winner is not None:
            dropped.append((block, winner, "label" if label else "lines"))
            continue
        seen[key] = block
        kept.append(block)
    return kept, dropped


# ---------------------------------------------------------------------------
# Environment classification
# ---------------------------------------------------------------------------

def suggested_canonical(env: str) -> str | None:
    base = env.rstrip("*")
    if base in CANONICAL_ENVS:
        return base
    if base in ALIAS_HINTS:
        return CANONICAL_BY_PREFIX.get(base[:3])
    return None


def is_theorem_like(env: str, declared: dict[str, str], alias_map: dict[str, str]) -> bool:
    base = env.rstrip("*")
    return (
        base in CANONICAL_ENVS
        or base in declared
        or base in alias_map
        or base in ALIAS_HINTS
    )


# ---------------------------------------------------------------------------
# Advisory ranking (§5.4)
# ---------------------------------------------------------------------------

def rank_candidates(blocks: list[Block], stripped: str) -> dict[int, dict[str, Any]]:
    """Score candidates; return ``{block index: {score, reasons}}``."""
    section_match = SECTION_RE.search(stripped)
    early_refs: set[str] = set()
    if section_match:
        head = stripped[: section_match.start()]
        for match in REF_RE.finditer(head):
            for label in match.group(1).split(","):
                label = label.strip()
                if label:
                    early_refs.add(label)

    total = len(blocks)
    scored: dict[int, dict[str, Any]] = {}
    for position, block in enumerate(blocks):
        score = 0.0
        reasons: list[str] = []
        if section_match and any(label in early_refs for label in block.labels):
            score += 3.0
            reasons.append("label is \\ref'd before the first \\section")
        found = stripped.find(block.text)
        context = block.title + "\n" + block.text
        if found != -1:
            context = stripped[max(0, found - 300):found] + "\n" + context
        if MAIN_RESULT_RE.search(context):
            score += 2.0
            reasons.append('"main result" phrasing at or just before the block')
        if total > 1:
            score += position / (total - 1)
        if position == total - 1 and total > 1:
            reasons.append(
                "final theorem of the paper"
                if block.env == "theorem"
                else "final statement block of the paper"
            )
        if len(block.text) >= LONG_BLOCK_CHARS:
            score += 0.5
        if block.env == "theorem":
            score += 0.5
        scored[position] = {"score": round(score, 4), "reasons": reasons}
    return scored


# ---------------------------------------------------------------------------
# Resolution
# ---------------------------------------------------------------------------

def read_paper(path: Path) -> tuple[str, str, list[dict[str, str]]]:
    """Return (text, sha256, warnings). Non-UTF-8 input is transcoded, not fatal."""
    data = path.read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    warnings: list[dict[str, str]] = []
    try:
        return data.decode("utf-8"), digest, warnings
    except UnicodeDecodeError as exc:
        for encoding in ("cp1252", "latin-1"):
            try:
                text = data.decode(encoding)
            except UnicodeDecodeError:
                continue
            warnings.append(
                {
                    "code": "not_utf8",
                    "message": (
                        f"{path.name} is not valid UTF-8 (byte {exc.start} on decode); "
                        f"read as {encoding} for this diagnostic. The kernel reads the "
                        f"paper with fs::read_to_string and hard-errors on non-UTF-8 "
                        f"input, so the file must be transcoded to UTF-8 before a run "
                        f"can use it."
                    ),
                }
            )
            return text, digest, warnings
        raise SystemExit(f"{path}: cannot decode as UTF-8, cp1252 or latin-1")


def resolve(
    *,
    paper_path: Path,
    paper_name: str | None = None,
    main_result_envs: list[str] | None = None,
    explicit_env_map: dict[str, str] | None = None,
    kernel_cmd: list[str] | None = None,
) -> dict[str, Any]:
    main_envs = list(main_result_envs or DEFAULT_MAIN_RESULT_ENVS)
    explicit_env_map = dict(explicit_env_map or {})

    raw, digest, file_warnings = read_paper(paper_path)

    auto_alias_map = build_alias_map(raw)
    rewrites: dict[str, int] = {}
    if explicit_env_map:
        raw, rewrites = rewrite_paper(raw, explicit_env_map)

    declared = declared_envs(raw)

    window_start, window_end, window_applied = document_window(raw)
    stripped, offsets = strip_with_offsets(raw[window_start:window_end], window_start)
    lines = LineIndex(raw)
    masked = mask_comments(raw)

    env_set = set(main_envs)
    blocks, truncated_at = scan_blocks(stripped, offsets, lines, env_set)
    candidates_blocks, duplicate_drops = dedup_blocks(blocks)

    rejected: list[dict[str, Any]] = []
    if not window_applied and DOC_BEGIN in raw:
        file_warnings.append(
            {
                "code": "document_window_absent",
                "message": (
                    "\\begin{document} is present but no \\end{document} follows it "
                    + (
                        "(the first \\end{document} occurs earlier in the file), "
                        if DOC_END in raw
                        else ""
                    )
                    + "so the whole file including the preamble is scanned."
                ),
            }
        )

    file_warnings.extend(input_warnings(stripped, offsets, lines))
    env_counts = count_env_uses(stripped)
    file_warnings.extend(
        newtheorem_warnings(raw, masked, lines, declared, auto_alias_map, env_counts)
    )

    rejected.extend(duplicate_rejections(duplicate_drops))
    block_level = block_rejections(
        raw=raw,
        masked=masked,
        stripped=stripped,
        offsets=offsets,
        lines=lines,
        window=(window_start, window_end, window_applied),
        candidates=candidates_blocks,
        main_envs=env_set,
        declared=declared,
        alias_map=auto_alias_map,
        truncated_at=truncated_at,
        file_warnings=file_warnings,
        dropped=duplicate_drops,
    )
    rejected.extend(block_level)
    rejected.extend(
        spaced_marker_rejections(stripped, offsets, lines, declared, auto_alias_map)
    )
    nested_labels = {
        entry["label"]
        for entry in block_level
        if entry["reason_code"] == "nested_in_candidate" and entry["label"]
    }
    rejected.extend(label_rejections(candidates_blocks, stripped, offsets, nested_labels))
    rejected.sort(key=lambda entry: (entry["start_line"], entry["reason_code"]))

    scored = rank_candidates(candidates_blocks, stripped)
    order = sorted(
        range(len(candidates_blocks)),
        key=lambda position: (-scored[position]["score"], position),
    )
    rank_of = {position: rank for rank, position in enumerate(order, start=1)}

    candidates: list[dict[str, Any]] = []
    for position, block in enumerate(candidates_blocks):
        label = block.labels[0] if block.labels else None
        entry: dict[str, Any] = {
            "key": target_key(label, block.start_line, block.end_line),
            "tex_label": label,
            "env": block.env,
            "start_line": block.start_line,
            "end_line": block.end_line,
            "text": block.text,
            "preselected": bool(
                scored[position]["score"] >= RANK_PRESELECT_THRESHOLD
                or rank_of[position] == 1
            ),
            "rank": rank_of[position],
            "rank_reasons": scored[position]["reasons"],
            "first_class": label is not None,
        }
        if label is None:
            entry["note"] = (
                "unlabeled: cannot be extended later via add-targets; add a \\label "
                "and re-upload to fix"
            )
        candidates.append(entry)

    available_labels = sorted({label for block in blocks for label in block.labels})

    envs_present = collect_envs_present(
        counts=env_counts,
        declared=declared,
        alias_map=auto_alias_map,
        explicit_map=explicit_env_map,
        main_envs=env_set,
    )

    scan_truncated = None
    if truncated_at is not None:
        scan_truncated = {
            "line": lines.line_of(truncated_at),
            "message": (
                f"a \\begin{{ with no closing }} at line {lines.line_of(truncated_at)} "
                f"abandons the rest of the scan; blocks after that line were never "
                f"examined by the resolver and cannot be diagnosed here."
            ),
        }

    if not candidates and not rejected:
        file_warnings.append(
            {
                "code": "no_recognizable_envs",
                "message": (
                    "no main-result candidates, and no theorem-like block explains why: "
                    "this paper uses no environment the resolver recognizes. We cannot "
                    "guess which prose states your theorem. Declare your results in "
                    "\\begin{theorem} / \\begin{corollary} (or map an existing "
                    "environment onto one), then re-resolve."
                ),
            }
        )

    result = {
        "paper_sha256": digest,
        "paper_name": paper_name or paper_path.name,
        "normalization": {
            "alias_map": auto_alias_map,
            "rewrites": rewrites,
            "explicit_env_map": explicit_env_map,
        },
        "main_result_envs": main_envs,
        "envs_present": envs_present,
        "candidates": candidates,
        "available_labels": available_labels,
        "rejected_blocks": rejected,
        "file_warnings": file_warnings,
        "scan_truncated": scan_truncated,
    }

    if kernel_cmd:
        check_against_kernel(
            result=result,
            kernel_cmd=kernel_cmd,
            paper_path=paper_path,
            rewritten_text=raw if explicit_env_map else None,
            main_envs=main_envs,
        )
    return result


def count_env_uses(stripped: str) -> dict[str, int]:
    """How often each environment is opened inside the scanned window."""
    counts: dict[str, int] = {}
    index = 0
    while True:
        found = stripped.find(BEGIN_MARKER, index)
        if found == -1:
            break
        env_start = found + len(BEGIN_MARKER)
        env_end = stripped.find("}", env_start)
        if env_end == -1:
            break
        env = stripped[env_start:env_end].strip().lower()
        if env:
            counts[env] = counts.get(env, 0) + 1
        index = env_end + 1
    return counts


def declared_envs(text: str) -> dict[str, str]:
    """``\\newtheorem`` declarations: env name -> declared title (may be empty)."""
    declared: dict[str, str] = {}
    for match in NEWTHM_RE.finditer(text):
        alias = match.group("alias").strip()
        if alias:
            declared.setdefault(alias, match.group("title").strip())
    return declared


def input_warnings(
    stripped: str, offsets: list[int], lines: LineIndex
) -> list[dict[str, str]]:
    warnings: list[dict[str, str]] = []
    for match in INPUT_RE.finditer(stripped):
        line = lines.line_of(offsets[match.start()])
        warnings.append(
            {
                "code": "input_directives",
                "message": (
                    f"\\{match.group(1)}{{{match.group(2)}}} at line {line}: included "
                    f"files are not scanned - inline the content or upload a flattened "
                    f".tex."
                ),
            }
        )
    return warnings


def newtheorem_warnings(
    raw: str,
    masked: str,
    lines: LineIndex,
    declared: dict[str, str],
    alias_map: dict[str, str],
    counts: dict[str, int],
) -> list[dict[str, str]]:
    warnings: list[dict[str, str]] = []
    for match in NEWTHM_RE.finditer(raw):
        if "\0" in masked[match.start():match.end()]:
            continue
        alias = match.group("alias").strip()
        title = match.group("title").strip()
        if alias in CANONICAL_ENVS or alias in alias_map:
            continue
        if not counts.get(alias.lower()):
            # Declared but never opened: it cannot be hiding the author's
            # theorem, and a fix-it here would send them editing for nothing.
            continue
        line = lines.line_of(match.start())
        hint = suggested_canonical(alias)
        fix = (
            f"if {alias} blocks are theorems, add mapping {alias}={hint}"
            if hint
            # "state results" over-prompts: a paper's `conjecture` and `claim`
            # blocks genuinely state results, and mapping them would enrol
            # statements the run is not meant to prove as formalization
            # targets. The question is not whether the block asserts
            # something — it is whether the author wants it TARGETED.
            else f"if {alias} blocks state results that need to be specified "
            f"as formalization targets, map it onto one of "
            f"{', '.join(TEX_STATEMENT_ENVS)}"
        )
        warnings.append(
            {
                "code": "unmapped_newtheorem",
                "message": (
                    f"\\newtheorem{{{alias}}}{{{title}}} at line {line} declares an "
                    f"environment we do not recognize; {fix}."
                ),
            }
        )
    return warnings


def duplicate_rejections(
    drops: Iterable[tuple[Block, Block, str]]
) -> list[dict[str, Any]]:
    rejections = []
    for block, winner, kind in drops:
        if kind == "label":
            label = block.labels[0]
            rejections.append(
                _rejection(
                    block.env, block.start_line, block.end_line, "duplicate_label", label,
                    f"the label {label!r} already binds the block at lines "
                    f"{winner.start_line}-{winner.end_line}; the first block carrying a "
                    f"label wins and this one is deduped away. Give this block its own "
                    f"label.",
                )
            )
            continue
        # Unlabeled blocks are identified by their line range alone, so two of
        # them on the same lines are one target as far as the resolver can tell.
        rejections.append(
            _rejection(
                block.env, block.start_line, block.end_line, "duplicate_line_key", None,
                f"this block is unlabeled, so it is identified only by its line range; "
                f"the {winner.env} block on the same lines "
                f"({winner.start_line}-{winner.end_line}) already claims the key "
                f"lines:{winner.start_line}-{winner.end_line}, and only the first is "
                f"kept. Put the two blocks on separate lines, or give this one a "
                f"\\label.",
            )
        )
    return rejections


def label_rejections(
    candidates: list[Block],
    stripped: str,
    offsets: list[int],
    exclude_labels: set[str],
) -> list[dict[str, Any]]:
    """R4: labels that exist but do not bind their block."""
    rejections: list[dict[str, Any]] = []
    for block in candidates:
        direct = direct_labels(block.text)
        for ordinal, label in enumerate(block.labels[1:], start=2):
            if label in exclude_labels:
                continue  # already explained as a nested block's own label
            if label not in direct:
                # A label belonging to a nested environment (an equation, say)
                # was never competing to name the theorem. Telling the author to
                # move it first would be an edit for nothing.
                continue
            rejections.append(
                {
                    "env": block.env,
                    "label": label,
                    "start_line": block.start_line,
                    "end_line": block.end_line,
                    "reason_code": "label_not_first",
                    "block_accepted": True,
                    "message": (
                        f"this block is a candidate, but under the label "
                        f"{block.labels[0]!r}: {label!r} is its "
                        f"{ordinal_word(ordinal)} label and only the first binds. "
                        f"Configuring the run with {label!r} resolves it without a line "
                        f"range, and the target is then dropped at init - put "
                        f"{label!r} first if it is the one you reference."
                    ),
                }
            )
    for block in candidates:
        if block.labels:
            continue
        tail_start = bisect.bisect_left(offsets, block.end_off)
        if tail_start >= len(offsets):
            continue
        match = LEADING_LABEL_RE.match(stripped[tail_start:])
        if not match:
            continue
        label = match.group(1).strip()
        if not label:
            continue
        rejections.append(
            {
                "env": block.env,
                "label": label,
                "start_line": block.start_line,
                "end_line": block.end_line,
                "reason_code": "label_outside_env",
                "block_accepted": True,
                "message": (
                    f"\\label{{{label}}} sits after \\end{{{block.env}}}, where the "
                    f"resolver cannot see it; the block is a candidate but only as an "
                    f"unlabeled one. Move the \\label inside the environment."
                ),
            }
        )
    return rejections


def ordinal_word(number: int) -> str:
    suffix = {1: "st", 2: "nd", 3: "rd"}.get(
        number if number % 100 not in (11, 12, 13) else 0, "th"
    )
    return f"{number}{suffix}"


def spaced_marker_rejections(
    stripped: str,
    offsets: list[int],
    lines: LineIndex,
    declared: dict[str, str],
    alias_map: dict[str, str],
) -> list[dict[str, Any]]:
    """Blocks hidden by whitespace between ``\\begin`` and its brace."""
    rejections: list[dict[str, Any]] = []
    for match in SPACED_BEGIN_RE.finditer(stripped):
        env = match.group(1).strip().lower()
        if not is_theorem_like(env, declared, alias_map):
            continue
        line = lines.line_of(offsets[match.start()])
        end = SPACED_END_RE.search(stripped, match.end())
        tail = stripped[match.start():end.end()] if end else stripped[match.start():]
        labels = extract_labels(tail)
        rejections.append(
            _rejection(
                env, line, line, "spaced_begin_marker", labels[0] if labels else None,
                f"the scan matches the literal string \\begin{{, so the space in "
                f"`{match.group(0).strip()}` at line {line} hides this block from it "
                f"entirely - it is not a candidate and no other rule applies. LaTeX "
                f"accepts the space; the resolver does not. Remove it.",
            )
        )
    return rejections


def block_rejections(
    *,
    raw: str,
    masked: str,
    stripped: str,
    offsets: list[int],
    lines: LineIndex,
    window: tuple[int, int, bool],
    candidates: list[Block],
    main_envs: set[str],
    declared: dict[str, str],
    alias_map: dict[str, str],
    truncated_at: int | None,
    file_warnings: list[dict[str, str]],
    dropped: list[tuple[Block, Block, str]],
) -> list[dict[str, Any]]:
    window_start, window_end, window_applied = window
    candidate_starts = {block.begin_off for block in candidates}
    explained_starts = {block.begin_off for block, _, _ in dropped}
    # A block the scan matched but then deduped away still moved the cursor, so
    # it hides nested blocks exactly as a candidate does.
    spans = [(block.begin_off, block.end_off, block, True) for block in candidates]
    spans += [(block.begin_off, block.end_off, block, False) for block, _, _ in dropped]

    rejections: list[dict[str, Any]] = []
    seen_offsets: set[int] = set()

    # In-window, uncommented occurrences, named exactly as the kernel sees them.
    occurrences: list[tuple[int, str, int | None]] = []
    index = 0
    while True:
        found = stripped.find(BEGIN_MARKER, index)
        if found == -1:
            break
        env_start = found + len(BEGIN_MARKER)
        env_end = stripped.find("}", env_start)
        if env_end == -1:
            break
        name = stripped[env_start:env_end].strip()
        if MALFORMED_ENV_RE.search(name):
            # The kernel takes everything up to the next `}` as the env name and
            # resumes there, so whatever lies in between is never examined.
            file_warnings.append(
                {
                    "code": "malformed_begin",
                    "message": (
                        f"the \\begin{{ at line {lines.line_of(offsets[found])} has no "
                        f"closing brace on its own line; the scan reads everything up to "
                        f"the brace on line {lines.line_of(offsets[env_end])} as one "
                        f"environment name and resumes there, so any block in that span "
                        f"was skipped and cannot be diagnosed. Close the brace."
                    ),
                }
            )
        else:
            occurrences.append((offsets[found], name, found))
        index = env_end + 1

    # Commented-out or out-of-window occurrences, from the raw text.
    index = 0
    while True:
        found = raw.find(BEGIN_MARKER, index)
        if found == -1:
            break
        index = found + len(BEGIN_MARKER)
        env_end = raw.find("}", index)
        if env_end == -1:
            break
        index = env_end + 1
        commented = "\0" in masked[found:env_end + 1]
        in_window = (not window_applied) or (window_start <= found < window_end)
        if commented or not in_window:
            occurrences.append((found, raw[found + len(BEGIN_MARKER):env_end].strip(), None))

    for offset, raw_env, stripped_pos in sorted(occurrences):
        if offset in seen_offsets:
            continue
        seen_offsets.add(offset)
        if offset in explained_starts:
            continue  # already rejected with a more specific reason
        env = raw_env.lower()
        if truncated_at is not None and offset > truncated_at:
            continue  # §1.3(a): the scan never got here; we cannot say why.
        commented = "\0" in masked[offset:offset + len(BEGIN_MARKER) + len(raw_env) + 1]
        in_window = (not window_applied) or (window_start <= offset < window_end)
        line = lines.line_of(offset)
        theorem_like = is_theorem_like(env, declared, alias_map)
        if stripped_pos is not None:
            stop, labels = _extent_labels(stripped, stripped_pos, raw_env)
            end_line = lines.line_of(offsets[stop - 1]) if stop is not None else line
        else:
            stop, labels = _extent_labels(raw, offset, raw_env)
            end_line = lines.line_of(stop - 1) if stop is not None else line
        label = labels[0] if labels else None

        # A candidate spelled in mixed case is unremarkable: it is matched
        # case-insensitively and closed by its own `\end`, like any other block.
        if offset in candidate_starts:
            continue

        if commented:
            if theorem_like:
                rejections.append(
                    _rejection(
                        env, line, end_line, "commented_out", label,
                        f"this \\begin{{{raw_env}}} is inside a % comment; lines are cut "
                        f"at the first unescaped % before scanning.",
                    )
                )
            continue

        if not in_window:
            if theorem_like:
                close_line = lines.line_of(window_end)
                commented_close = "\0" in masked[window_end:window_end + len(DOC_END)]
                rejections.append(
                    _rejection(
                        env, line, end_line, "outside_document_window", label,
                        f"this block lies outside the scanned window: only the span "
                        f"between \\begin{{document}} and the first \\end{{document}} "
                        f"(line {close_line}) is scanned"
                        + (
                            ", and that \\end{document} closes the window even though it "
                            "is commented out - delete it or move this block above it."
                            if commented_close
                            else "."
                        ),
                    )
                )
            continue

        enclosing, outer_is_candidate = next(
            (
                (block, is_candidate)
                for start, end, block, is_candidate in spans
                if start < offset < end
            ),
            (None, False),
        )
        if enclosing is not None:
            if theorem_like and outer_is_candidate:
                rejections.append(
                    _rejection(
                        env, line, end_line, "nested_in_candidate", label,
                        f"this \\begin{{{raw_env}}} sits inside the "
                        f"{enclosing.env} block at lines {enclosing.start_line}-"
                        f"{enclosing.end_line}; the scan jumps past a matched block, so "
                        f"nested statements are never seen. Move it out of that block.",
                    )
                )
            elif theorem_like:
                rejections.append(
                    _rejection(
                        env, line, end_line, "nested_in_dropped_block", label,
                        f"this \\begin{{{raw_env}}} sits inside the {enclosing.env} "
                        f"block at lines {enclosing.start_line}-{enclosing.end_line}, "
                        f"which the scan matched and then dropped as a duplicate. The "
                        f"cursor still jumped past it, so this block was never seen "
                        f"either - fixing the outer block's duplicate key is not enough, "
                        f"move this block out of it.",
                    )
                )
            continue

        if env in main_envs:
            walked = (
                walk_block_end(stripped, stripped_pos, raw_env, env)
                if stripped_pos is not None
                else ("open", -1, -1, "")
            )
            if walked[0] == "closed":
                rejections.append(
                    _rejection(
                        env, line, end_line, "undiagnosed_block", label,
                        f"this \\begin{{{raw_env}}} closes normally inside the scanned "
                        f"window yet did not become a candidate, and no rule we mirror "
                        f"accounts for it. We would rather say so than invent a reason; "
                        f"please report this paper.",
                    )
                )
                continue
            spaced_end_line = None
            if stripped_pos is not None:
                spaced = SPACED_END_RE.search(stripped, stripped_pos)
                if spaced is not None and spaced.group(1).strip().lower() == env:
                    spaced_end_line = lines.line_of(offsets[spaced.start()])
            # The block did not close. An `\end` naming this environment in
            # another spelling, closing nothing this block opened, is the one
            # interesting way that happens: the pair is invalid LaTeX, and the
            # scan stops there rather than running on to a same-spelling `\end`
            # and swallowing the blocks in between.
            if walked[0] == "mismatch":
                _, mismatch_off, _, mismatch_name = walked
                mismatch_line = lines.line_of(offsets[mismatch_off])
                rejections.append(
                    _rejection(
                        env, line, mismatch_line, "env_case_end_mismatch", label,
                        f"\\begin{{{raw_env}}} at line {line} is closed by "
                        f"\\end{{{mismatch_name}}} at line {mismatch_line}, which opens "
                        f"nothing inside it. LaTeX environment names are case-sensitive, "
                        f"so that does not close the block and it is dropped - the scan "
                        f"stops at a mismatched \\end rather than run past it and swallow "
                        f"the blocks that follow. Spell both ends the same way.",
                    )
                )
            else:
                rejections.append(
                    _rejection(
                        env, line, line, "unterminated_env", label,
                        f"no \\end{{{raw_env}}} follows this \\begin{{{raw_env}}} inside "
                        f"the scanned window, so the block is dropped silently. "
                        + (
                            f"There is an \\end with a space before its brace at line "
                            f"{spaced_end_line}; the scan matches the literal "
                            "\\end{, so remove that space."
                            if spaced_end_line is not None
                            else f"Add the matching \\end{{{raw_env}}}."
                        ),
                    )
                )
            continue

        base = env.rstrip("*")
        if env != base and (base in CANONICAL_ENVS or base in declared or base in ALIAS_HINTS):
            rejections.append(
                _rejection(
                    env, line, end_line, "starred_env", label,
                    f"starred environments are not recognized: the name is matched "
                    f"literally, so {env!r} never matches. Use \\begin{{{base}}} "
                    f"(numbered) instead.",
                )
            )
            continue

        if env in CANONICAL_ENVS:
            rejections.append(
                _rejection(
                    env, line, end_line, "env_not_main_result", label,
                    f"{env} blocks are not main-result candidates in this run; only "
                    f"{' and '.join(sorted(main_envs))} are. Toggle {env} on in the "
                    f"environments control if it states a main result, or select the "
                    f"theorem that states it.",
                )
            )
            continue

        if env in declared or env in alias_map or env in ALIAS_HINTS:
            hint = alias_map.get(env) or suggested_canonical(env)
            fix = (
                f"add mapping {env}={hint} to normalize it"
                if hint
                else f"map it onto one of {', '.join(TEX_STATEMENT_ENVS)}"
            )
            rejections.append(
                _rejection(
                    env, line, end_line, "alias_unnormalized", label,
                    f"\\begin{{{raw_env}}} is an alias environment; the resolver matches "
                    f"environment names literally and does not read \\newtheorem, so it "
                    f"yields no candidates - {fix}.",
                )
            )
            continue

    return rejections


def _extent_labels(text: str, start: int, raw_env: str) -> tuple[int | None, list[str]]:
    """Where the author's block ends and which labels it carries (diagnostic only)."""
    for marker in ("\\end{" + raw_env + "}", "\\end{" + raw_env.lower() + "}"):
        end = text.find(marker, start)
        if end != -1:
            stop = end + len(marker)
            return stop, extract_labels(text[start:stop])
    line_end = text.find("\n", start)
    tail = text[start:] if line_end == -1 else text[start:line_end]
    return None, extract_labels(tail)


def _rejection(
    env: str, start: int, end: int, code: str, label: str | None, message: str
) -> dict[str, Any]:
    return {
        "env": env,
        "label": label,
        "start_line": start,
        "end_line": end,
        "reason_code": code,
        "message": message,
    }


def collect_envs_present(
    *,
    counts: dict[str, int],
    declared: dict[str, str],
    alias_map: dict[str, str],
    explicit_map: dict[str, str],
    main_envs: set[str],
) -> list[dict[str, Any]]:
    names = set(counts) | {name.lower() for name in declared}
    entries: list[dict[str, Any]] = []
    for name in names:
        canonical = None
        if name in CANONICAL_ENVS:
            canonical = name
        elif name in explicit_map:
            canonical = explicit_map[name]
        elif name in alias_map:
            canonical = alias_map[name]
        entry: dict[str, Any] = {
            "env": name,
            "count": counts.get(name, 0),
            "canonical": canonical,
            "main_result": bool(canonical and canonical in main_envs),
            "declared": name in {key.lower() for key in declared},
        }
        title = next(
            (value for key, value in declared.items() if key.lower() == name), None
        )
        if title:
            entry["declared_title"] = title
        if canonical is None:
            hint = suggested_canonical(name)
            if hint:
                entry["suggested_canonical"] = hint
        entries.append(entry)
    entries.sort(
        key=lambda entry: (
            not entry["main_result"],
            entry["canonical"] is None,
            not entry["declared"],
            -entry["count"],
            entry["env"],
        )
    )
    return entries


# ---------------------------------------------------------------------------
# Kernel cross-check (mirror-drift guard)
# ---------------------------------------------------------------------------

def kernel_request(kernel_cmd: list[str], payload: dict[str, Any]) -> dict[str, Any]:
    process = subprocess.run(
        kernel_cmd,
        input=json.dumps(payload),
        capture_output=True,
        text=True,
        timeout=120,
    )
    if process.returncode != 0:
        raise RuntimeError(f"kernel CLI failed: {process.stderr.strip()[:400]}")
    return json.loads(process.stdout)


def kernel_honors_env_knob(kernel_cmd: list[str], paper_path: Path) -> bool:
    """Does this kernel binary understand ``main_result_envs``?

    The request enum has no ``deny_unknown_fields``, so a kernel built before
    the knob accepts the field, ignores it, and answers under the default env
    set. Comparing a widened scan against that answer would report a
    disagreement that is entirely our own fault. A knob-aware kernel validates
    the env list, so an impossible env name is a capability probe: an error
    means the knob is understood, a success means the field was ignored.
    """
    response = kernel_request(
        kernel_cmd,
        {
            "action": "resolve_main_result_targets",
            "paper_path": str(paper_path.resolve()),
            "raw_targets": None,
            "raw_labels": None,
            "main_result_envs": ["__trellis_env_knob_probe__"],
        },
    )
    return response.get("status") != "resolve_main_result_targets_ok"


def kernel_targets(
    kernel_cmd: list[str],
    paper_path: Path,
    main_envs: list[str],
) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "action": "resolve_main_result_targets",
        "paper_path": str(paper_path.resolve()),
        "raw_targets": None,
        "raw_labels": None,
    }
    if list(main_envs) != DEFAULT_MAIN_RESULT_ENVS:
        payload["main_result_envs"] = list(main_envs)
    response = kernel_request(kernel_cmd, payload)
    if response.get("status") != "resolve_main_result_targets_ok":
        raise RuntimeError(f"kernel CLI status {response.get('status')!r}")
    return response["output"]


def check_against_kernel(
    *,
    result: dict[str, Any],
    kernel_cmd: list[str],
    paper_path: Path,
    rewritten_text: str | None,
    main_envs: list[str],
) -> None:
    temp_dir = None
    try:
        target = paper_path
        if rewritten_text is not None:
            temp_dir = tempfile.TemporaryDirectory(prefix="resolve-targets-")
            target = Path(temp_dir.name) / paper_path.name
            target.write_text(rewritten_text, encoding="utf-8")
        if list(main_envs) != DEFAULT_MAIN_RESULT_ENVS and not kernel_honors_env_knob(
            kernel_cmd, target
        ):
            result["file_warnings"].append(
                {
                    "code": "kernel_knob_unsupported",
                    "message": (
                        "this kernel binary predates the main_result_envs knob: it "
                        "accepts the field, ignores it, and resolves under the default "
                        f"set ({', '.join(DEFAULT_MAIN_RESULT_ENVS)}). The candidates "
                        f"below were scanned under {', '.join(main_envs)} and could not "
                        "be cross-checked; rebuild the kernel to verify them."
                    ),
                }
            )
            return
        output = kernel_targets(kernel_cmd, target, main_envs)
    except Exception as exc:  # noqa: BLE001 - the guard must never break resolution
        result["file_warnings"].append(
            {
                "code": "kernel_check_unavailable",
                "message": f"could not cross-check against the kernel resolver: {exc}",
            }
        )
        return
    finally:
        if temp_dir is not None:
            temp_dir.cleanup()

    mine = [
        (entry["start_line"], entry["end_line"], entry["tex_label"])
        for entry in result["candidates"]
    ]
    theirs = [
        (entry.get("start_line"), entry.get("end_line"), entry.get("tex_label"))
        for entry in output.get("targets", [])
    ]
    if mine != theirs or result["available_labels"] != sorted(
        output.get("available_labels", [])
    ):
        result["file_warnings"].append(
            {
                "code": "mirror_disagreement",
                "message": (
                    "the diagnostic scanner and the kernel resolver disagree about this "
                    f"paper (scanner: {mine}, kernel: {theirs}); the kernel is "
                    "authoritative, so treat the diagnostics below as unreliable and "
                    "report this."
                ),
            }
        )
        result["candidates_kernel"] = output.get("targets", [])


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def parse_env_map(specs: list[str], parser: argparse.ArgumentParser) -> dict[str, str]:
    mapping: dict[str, str] = {}
    for spec in specs:
        alias, sep, canonical = spec.partition("=")
        alias, canonical = alias.strip(), canonical.strip().lower()
        if not sep or not alias or not canonical:
            parser.error(f"--env-map expects ALIAS=CANONICAL, got {spec!r}")
        if canonical not in CANONICAL_ENVS:
            parser.error(
                f"--env-map target {canonical!r} is not a kernel-canonical env "
                f"({', '.join(TEX_STATEMENT_ENVS)})"
            )
        mapping[alias] = canonical
    return mapping


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Resolve main-result candidates and diagnose the rejected blocks.",
    )
    parser.add_argument("paper", type=Path, help="Paper .tex to scan")
    parser.add_argument("--out", type=Path, help="Write JSON here instead of stdout")
    parser.add_argument("--paper-name", help="Name to record in the output")
    parser.add_argument(
        "--main-result-envs",
        help=(
            "Comma-separated main-result env set (default: "
            f"{','.join(DEFAULT_MAIN_RESULT_ENVS)})"
        ),
    )
    parser.add_argument(
        "--env-map",
        action="append",
        default=[],
        metavar="ALIAS=CANONICAL",
        help="Alias mapping to apply before scanning, repeatable.",
    )
    parser.add_argument(
        "--kernel-cmd",
        help=(
            "Kernel CLI to cross-check candidates against (mirror-drift guard); "
            "shell-style argument string."
        ),
    )
    args = parser.parse_args(argv)

    if not args.paper.is_file():
        parser.error(f"paper not found: {args.paper}")

    main_envs = DEFAULT_MAIN_RESULT_ENVS
    if args.main_result_envs:
        main_envs = [
            env.strip().lower() for env in args.main_result_envs.split(",") if env.strip()
        ]
        unknown = [env for env in main_envs if env not in CANONICAL_ENVS]
        if unknown:
            parser.error(
                f"--main-result-envs entries must be within "
                f"{', '.join(TEX_STATEMENT_ENVS)}; got {', '.join(unknown)}"
            )
        if not main_envs:
            parser.error("--main-result-envs must name at least one environment")

    env_map = parse_env_map(args.env_map, parser)
    kernel_cmd = None
    if args.kernel_cmd:
        import shlex

        kernel_cmd = shlex.split(args.kernel_cmd)

    result = resolve(
        paper_path=args.paper,
        paper_name=args.paper_name,
        main_result_envs=main_envs,
        explicit_env_map=env_map,
        kernel_cmd=kernel_cmd,
    )
    text = json.dumps(result, indent=2, ensure_ascii=False) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
