#!/usr/bin/env python3
"""Derive challenge_targets.json from a downloaded lean-eval problem directory.

The operator downloads a problem from lean-lang.org/eval (Challenge.lean,
ChallengeDeps.lean, config.json, lean-toolchain, lakefile.toml, README.md)
and runs this importer on that directory. The prescribed Lean text is never
hand-transcribed: a human copy step in front of a byte-exactness check is the
failure mode this script exists to remove.

Extraction:

* Theorem statements come from Challenge.lean, with the proof body stripped
  at the top-level ``:=`` boundary and re-terminated per FILESPEC (the final
  line ends ``:=`` or ``by``). The theorem name set must equal config.json's
  ``theorem_names``.
* Every ``def`` in ChallengeDeps.lean becomes a def target, kept whole but
  canonicalized at the ``:=`` boundary so the emitted text is
  FILESPEC-compatible (a non-``by`` tail after ``:=`` moves to the next line
  indented two spaces).
* ``namespace``/``end`` wrappers are removed from the emitted text and
  recorded as a dot-joined ``namespace_context`` so each declaration is
  canonical for the in-Tablet byte comparison.
* Toolchain pins are read from lean-toolchain and the lakefile's mathlib
  ``rev`` and emitted as MATHLIB_TOOLCHAIN / MATHLIB_REV.

Re-running on the same download produces byte-identical output.

Usage:
    import_challenge_targets.py <problem_dir> [--out PATH]
        Default --out is <problem_dir>/challenge_targets.json.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

SCHEMA_VERSION = 1

REQUIRED_FILES = (
    "Challenge.lean",
    "ChallengeDeps.lean",
    "config.json",
    "lean-toolchain",
    "lakefile.toml",
)
OPTIONAL_FILES = ("README.md",)

DECL_MODIFIERS = ("noncomputable", "private", "protected", "unsafe", "partial")
_MODIFIER_ALT = "|".join(DECL_MODIFIERS)
DECL_START_RE = re.compile(
    rf"^(?:(?:{_MODIFIER_ALT})\s+)*(theorem|def)\s+([^\s:({{\[⟨]+)"
)
MODIFIER_ONLY_RE = re.compile(rf"^(?:{_MODIFIER_ALT})(?:\s+(?:{_MODIFIER_ALT}))*$")

BRACKET_OPEN = {"(": ")", "[": "]", "{": "}", "⟨": "⟩"}
BRACKET_CLOSE = {v: k for k, v in BRACKET_OPEN.items()}


class ImporterError(Exception):
    """Malformed problem input; reported to stderr with nonzero exit."""


@dataclass
class Declaration:
    kind: str  # "theorem" | "def"
    name: str
    text: str  # dedented, blank-trimmed declaration text
    namespace_context: str  # dot-joined namespace stack, "" when none


def _advance_comment_depth(line: str, depth: int) -> int:
    """Track Lean block-comment nesting across a line.

    At depth 0 a ``--`` token ends scanning (line comment). ``/-`` opens and
    ``-/`` closes nested block comments (``/--`` and ``/-!`` both open).
    """
    i = 0
    while i < len(line) - 1:
        pair = line[i : i + 2]
        if pair == "/-":
            depth += 1
            i += 2
        elif pair == "-/":
            depth = max(0, depth - 1)
            i += 2
        elif depth == 0 and pair == "--":
            break
        else:
            i += 1
    return depth


def _dedent_and_trim(lines: list[str]) -> str:
    while lines and not lines[0].strip():
        lines = lines[1:]
    while lines and not lines[-1].strip():
        lines = lines[:-1]
    indents = [len(ln) - len(ln.lstrip()) for ln in lines if ln.strip()]
    cut = min(indents) if indents else 0
    return "\n".join(ln[cut:] if ln.strip() else "" for ln in lines)


def extract_declarations(text: str, source_name: str) -> list[Declaration]:
    """Extract theorem/def declarations with their namespace contexts.

    A declaration starts at its keyword line (plus contiguous immediately
    preceding ``@[...]`` attribute or bare-modifier lines) and ends before the
    next declaration prelude, doc-comment opener, ``namespace``/``end`` line,
    or EOF.
    """
    lines = text.split("\n")
    ns_stack: list[str] = []
    decls: list[Declaration] = []
    comment_depth = 0
    current: dict | None = None  # {kind,name,ns,start,end}
    prelude_start: int | None = None

    def close_current(last_line_exclusive: int) -> None:
        nonlocal current
        if current is None:
            return
        body = _dedent_and_trim(lines[current["start"] : last_line_exclusive])
        decls.append(
            Declaration(
                kind=current["kind"],
                name=current["name"],
                text=body,
                namespace_context=current["ns"],
            )
        )
        current = None

    for i, raw in enumerate(lines):
        in_comment = comment_depth > 0
        comment_depth = _advance_comment_depth(raw, comment_depth)
        if in_comment:
            continue  # comment continuation; stays inside any open declaration
        stripped = raw.strip()
        if not stripped:
            # Blank lines inside a declaration are trimmed later; a blank
            # line does break attribute/modifier prelude contiguity.
            prelude_start = None
            continue
        if stripped.startswith("/-"):
            close_current(i)
            prelude_start = None
            continue
        if stripped.startswith("--"):
            continue
        if stripped.startswith("@[") or MODIFIER_ONLY_RE.match(stripped):
            close_current(i)
            if prelude_start is None:
                prelude_start = i
            continue
        match = DECL_START_RE.match(stripped)
        if match:
            close_current(i)
            current = {
                "kind": match.group(1),
                "name": match.group(2),
                "ns": ".".join(ns_stack),
                "start": prelude_start if prelude_start is not None else i,
            }
            prelude_start = None
            continue
        prelude_start = None
        if stripped.startswith("namespace "):
            close_current(i)
            ns_stack.extend(stripped.split(None, 1)[1].split("."))
            continue
        if stripped == "end" or stripped.startswith("end "):
            close_current(i)
            parts = stripped.split(None, 1)
            if len(parts) == 2:
                ended = parts[1].split(".")
                if ns_stack[-len(ended) :] == ended:
                    del ns_stack[-len(ended) :]
            continue
        # Any other top-level line (imports, opens, a declaration body line).

    close_current(len(lines))
    if ns_stack:
        raise ImporterError(
            f"{source_name}: unclosed namespace block(s): {'.'.join(ns_stack)}"
        )
    return decls


def find_toplevel_assign(text: str) -> int | None:
    """Index of the ``:`` of the first ``:=`` outside brackets and comments."""
    depth = 0
    comment_depth = 0
    i = 0
    n = len(text)
    while i < n:
        pair = text[i : i + 2]
        if pair == "/-":
            comment_depth += 1
            i += 2
            continue
        if comment_depth > 0:
            if pair == "-/":
                comment_depth -= 1
                i += 2
            else:
                i += 1
            continue
        if pair == "--":
            nl = text.find("\n", i)
            i = n if nl == -1 else nl
            continue
        ch = text[i]
        if ch == '"':
            i += 1
            while i < n and text[i] != '"':
                i += 2 if text[i] == "\\" else 1
            i += 1
            continue
        if ch in BRACKET_OPEN:
            depth += 1
        elif ch in BRACKET_CLOSE:
            depth = max(0, depth - 1)
        elif depth == 0 and pair == ":=":
            return i
        i += 1
    return None


_BY_AFTER_ASSIGN_RE = re.compile(r"[ \t\n]*by(?![\w'!?])")


def strip_theorem_proof(decl: Declaration) -> str:
    """Drop the proof body, re-terminating per FILESPEC (``:=`` or ``by``)."""
    pos = find_toplevel_assign(decl.text)
    if pos is None:
        raise ImporterError(
            f"theorem {decl.name}: no top-level ':=' proof boundary found"
        )
    after = decl.text[pos + 2 :]
    match = _BY_AFTER_ASSIGN_RE.match(after)
    end = pos + 2 + (match.end() if match else 0)
    return decl.text[:end]


def canonicalize_def(decl: Declaration) -> str:
    """Re-break the ``:=`` line so the emitted def is FILESPEC-compatible.

    If the line carrying the top-level ``:=`` has a non-``by`` tail, the line
    is cut at ``:=`` (or after ``by`` when the tail starts with ``by`` and
    continues) and the remainder moves to the next line indented two spaces.
    """
    pos = find_toplevel_assign(decl.text)
    if pos is None:
        raise ImporterError(f"def {decl.name}: no top-level ':=' found")
    line_end = decl.text.find("\n", pos)
    if line_end == -1:
        line_end = len(decl.text)
    tail = decl.text[pos + 2 : line_end]
    if not tail.strip() or tail.strip() == "by":
        return decl.text
    by_match = re.match(r"[ \t]*by(?![\w'!?])", tail)
    cut = pos + 2 + (by_match.end() if by_match else 0)
    remainder = decl.text[cut:line_end].strip()
    return decl.text[:cut] + "\n  " + remainder + decl.text[line_end:]


def _sha256_hex(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _load_config(path: Path) -> list[str]:
    try:
        config = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise ImporterError(f"config.json is not valid JSON: {exc}") from exc
    names = config.get("theorem_names")
    if (
        not isinstance(names, list)
        or not names
        or not all(isinstance(n, str) for n in names)
    ):
        raise ImporterError(
            "config.json must contain a non-empty 'theorem_names' string list"
        )
    return names


def _load_lakefile(path: Path) -> tuple[str, str]:
    """Return (problem_id, mathlib rev) from lakefile.toml."""
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as exc:
        raise ImporterError(f"lakefile.toml is not valid TOML: {exc}") from exc
    problem_id = data.get("name")
    if not isinstance(problem_id, str) or not problem_id:
        raise ImporterError("lakefile.toml has no top-level 'name'")
    for req in data.get("require", []):
        if isinstance(req, dict) and req.get("name") == "mathlib":
            rev = req.get("rev")
            if not isinstance(rev, str) or not rev:
                raise ImporterError(
                    "lakefile.toml mathlib requirement has no 'rev'"
                )
            return problem_id, rev
    raise ImporterError("lakefile.toml has no [[require]] entry named 'mathlib'")


def build_spec(problem_dir: Path) -> dict:
    if not problem_dir.is_dir():
        raise ImporterError(f"not a directory: {problem_dir}")
    for filename in REQUIRED_FILES:
        if not (problem_dir / filename).is_file():
            raise ImporterError(f"missing required file: {filename}")

    theorem_names = _load_config(problem_dir / "config.json")
    problem_id, mathlib_rev = _load_lakefile(problem_dir / "lakefile.toml")
    toolchain = (problem_dir / "lean-toolchain").read_text(encoding="utf-8").strip()
    if not toolchain:
        raise ImporterError("lean-toolchain is empty")

    readme_path = problem_dir / "README.md"
    informal = (
        readme_path.read_text(encoding="utf-8").strip()
        if readme_path.is_file()
        else ""
    )

    hashed_files = sorted(
        name
        for name in REQUIRED_FILES + OPTIONAL_FILES
        if (problem_dir / name).is_file()
    )
    source_hashes = {name: _sha256_hex(problem_dir / name) for name in hashed_files}

    challenge_text = (problem_dir / "Challenge.lean").read_text(encoding="utf-8")
    deps_text = (problem_dir / "ChallengeDeps.lean").read_text(encoding="utf-8")
    theorems = {
        decl.name: decl
        for decl in extract_declarations(challenge_text, "Challenge.lean")
        if decl.kind == "theorem"
    }
    defs = [
        decl
        for decl in extract_declarations(deps_text, "ChallengeDeps.lean")
        if decl.kind == "def"
    ]

    if set(theorems) != set(theorem_names):
        raise ImporterError(
            "theorem name mismatch: Challenge.lean declares "
            f"{sorted(theorems)} but config.json theorem_names is "
            f"{sorted(theorem_names)}"
        )
    if not defs:
        raise ImporterError("ChallengeDeps.lean contains no def declarations")

    def target(decl: Declaration, lean: str, source_file: str, info: str) -> dict:
        return {
            "id": f"challenge:{decl.name}",
            "kind": decl.kind,
            "name": decl.name,
            "lean": lean,
            "namespace_context": decl.namespace_context,
            "informal": info,
            "provenance": {
                "problem_id": problem_id,
                "source_file": source_file,
                "source_sha256": source_hashes[source_file],
            },
        }

    targets = [
        target(theorems[name], strip_theorem_proof(theorems[name]),
               "Challenge.lean", informal)
        for name in theorem_names
    ] + [
        target(decl, canonicalize_def(decl), "ChallengeDeps.lean", "")
        for decl in defs
    ]

    return {
        "schema_version": SCHEMA_VERSION,
        "problem_id": problem_id,
        "toolchain": {
            "MATHLIB_TOOLCHAIN": toolchain,
            "MATHLIB_REV": mathlib_rev,
        },
        "source_hashes": source_hashes,
        "targets": targets,
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Import a lean-eval problem download into challenge_targets.json."
    )
    parser.add_argument("problem_dir", type=Path, help="downloaded problem directory")
    parser.add_argument(
        "--out",
        type=Path,
        default=None,
        help="output path (default: <problem_dir>/challenge_targets.json)",
    )
    args = parser.parse_args()

    try:
        spec = build_spec(args.problem_dir)
    except ImporterError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    out_path = args.out or args.problem_dir / "challenge_targets.json"
    out_path.write_text(
        json.dumps(spec, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    print(f"wrote {len(spec['targets'])} targets to {out_path}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
