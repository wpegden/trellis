#!/usr/bin/env python3
"""Bodiless declarations in the extracted Lean model.

A candidate can reach a clean extraction two ways: by restructuring the Rust
until the tools can model it, or by marking an item so the tools skip its
body.  The second still compiles and still passes every checker stage, but the
model it produces carries a declaration with no definition — the computation
became an assumption instead of something the artifact says anything about.

Nothing here reads the adaptation ledger, the source, or any list of source
markings: the fact is derived from the emitted model itself.  A declaration is
bodiless when the backend emits it as `axiom` or `opaque`; everything else the
backend emits carries a definition.

The generated Lean is what is read.  The LLBC alongside it is the backend's own
serialized form, and a bodiless item is only guaranteed to be observable there
as an absence; the Lean says it in one token at the head of the declaration.
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

# The two heads the Lean backend emits for a declaration with no definition.
BODILESS_KINDS = ("axiom", "opaque")

GENERATED_LEAN_NAME = "generated.lean"

_MODIFIER = r"(?:noncomputable|private|protected|partial|unsafe|nonrec|scoped|local)"
_DECLARATION_HEAD = re.compile(
    r"^(?:@\[[^\]\n]*\]\s*)*"
    rf"(?:{_MODIFIER}\s+)*"
    r"(axiom|opaque|def|abbrev|theorem|lemma|example|instance|structure|inductive|class)"
    r"(?![A-Za-z0-9_'!?])(.*)$"
)
_ITEM_PATH = re.compile(r"\[([^\]\n]+)\]")
# A Lean name segment: identifier characters plus the dotted qualification the
# backend writes into a boundary declaration's name.
_NAME_EDGE = r"[A-Za-z0-9_'.«»!?]"


def _code_view(text: str) -> tuple[list[str], dict[int, str]]:
    """Blank out comments and record each doc comment's bracketed item path.

    Returns the per-line code view (comment bytes replaced by spaces, so line
    numbers and column offsets are preserved) and, for every line that ends a
    doc comment, the first bracketed item path that doc comment carried.  The
    backend precedes each emitted declaration with `/-- [<item>]: … -/`, so the
    item path is the model's own statement of where the declaration came from.
    """
    lines = text.splitlines()
    code: list[str] = []
    doc_ends: dict[int, str] = {}
    depth = 0
    documenting = False
    buffer: list[str] = []
    for index, line in enumerate(lines):
        rendered: list[str] = []
        position = 0
        length = len(line)
        while position < length:
            if depth == 0 and line.startswith("--", position):
                rendered.append(" " * (length - position))
                position = length
                continue
            if line.startswith("/-", position):
                if depth == 0:
                    documenting = line.startswith("/--", position)
                    buffer = []
                depth += 1
                rendered.append("  ")
                position += 2
                continue
            if depth > 0 and line.startswith("-/", position):
                depth -= 1
                rendered.append("  ")
                position += 2
                if depth == 0 and documenting:
                    joined = "".join(buffer)
                    match = _ITEM_PATH.search(joined)
                    doc_ends[index] = match.group(1).strip() if match else ""
                    documenting = False
                continue
            if depth > 0:
                if documenting:
                    buffer.append(line[position])
                rendered.append(" ")
                position += 1
                continue
            rendered.append(line[position])
            position += 1
        code.append("".join(rendered))
        if depth > 0 and documenting:
            buffer.append("\n")
    return code, doc_ends


def _declared_name(rest: str, code: list[str], index: int) -> str:
    """The declared name, tolerating the backend's reflow onto the next line."""
    candidate = rest.strip()
    if not candidate:
        for follower in code[index + 1 :]:
            if follower.strip():
                candidate = follower.strip()
                break
    if not candidate:
        return ""
    token = candidate.split()[0]
    for boundary in ":({[":
        cut = token.find(boundary)
        if cut != -1:
            token = token[:cut]
    return token.strip().rstrip(".")


def scan_bodiless_declarations(text: str) -> list[dict[str, str]]:
    """Every declaration the model emits without a definition, in name order.

    Never raises: a file that does not parse as Lean simply yields nothing, so
    a scan can be reported alongside a check rather than failing it.
    """
    code, doc_ends = _code_view(text)
    pending_item = ""
    found: dict[tuple[str, str], dict[str, str]] = {}
    for index, line in enumerate(code):
        if index in doc_ends:
            pending_item = doc_ends[index]
        stripped = line.strip()
        if not stripped:
            continue
        head = _DECLARATION_HEAD.match(stripped)
        if head is None:
            # Any other code between the doc comment and a declaration means
            # the doc comment does not describe that declaration.
            pending_item = ""
            continue
        keyword, rest = head.group(1), head.group(2)
        if keyword in BODILESS_KINDS:
            name = _declared_name(rest, code, index)
            if name:
                found.setdefault((name, keyword), {
                    "name": name,
                    "kind": keyword,
                    "item_path": pending_item,
                })
        pending_item = ""
    return [found[key] for key in sorted(found, key=lambda key: (key[0].encode("utf-8"), key[1]))]


def declared_names(text: str, names: set[str]) -> list[str]:
    """Which of `names` this Lean text itself declares as bodiless."""
    return sorted(
        {row["name"] for row in scan_bodiless_declarations(text) if row["name"] in names},
        key=lambda value: value.encode("utf-8"),
    )


def referenced_names(text: str, names: set[str]) -> list[str]:
    """Which of `names` this Lean text mentions in code (comments excluded).

    One hop only: a name the text writes down.  What those declarations in turn
    reach is not followed.
    """
    code = "\n".join(_code_view(text)[0])
    hits = set()
    for name in names:
        pattern = re.compile(
            rf"(?<!{_NAME_EDGE}){re.escape(name)}(?!{_NAME_EDGE})"
        )
        if pattern.search(code):
            hits.add(name)
    return sorted(hits, key=lambda value: value.encode("utf-8"))


def unscanned_model(status: str, detail: str) -> dict[str, Any]:
    """A scan that read no model. It always says why: an empty reason is what
    a successful read carries, and the kernel holds both sides to that."""
    if not detail:
        raise ValueError("a scan that read no model must say why")
    return {
        "status": status,
        "read_from": GENERATED_LEAN_NAME,
        "detail": detail,
        "declarations": [],
    }


def read_model_bodiless_declarations(model_root: Path) -> dict[str, Any]:
    """Scan the generated Lean an extraction produced under `model_root`."""
    path = Path(model_root) / GENERATED_LEAN_NAME
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        # A model that cannot be read is reported as unread, not as a clean
        # scan: an empty list here would read as "nothing was suppressed".
        return unscanned_model("unavailable", f"cannot read the generated Lean: {error}")
    return {
        "status": "read",
        "read_from": GENERATED_LEAN_NAME,
        "detail": "",
        "declarations": scan_bodiless_declarations(text),
    }
