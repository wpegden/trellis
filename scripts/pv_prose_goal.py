#!/usr/bin/env python3
"""Parse the deliberately narrow CommonMark surface used by PV prose goals."""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path


_RUST_IDENTIFIER = r"[A-Za-z_][A-Za-z0-9_]*"
_FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")
_ATX = re.compile(r"^ {0,3}(#{1,6})(?:[ \t]+(.*?))?[ \t]*$")
_TARGET = re.compile(
    rf"`(?P<name>{_RUST_IDENTIFIER})`(?:[ \t]+\[(?P<resolution>decide|prove)\])?"
)
_PLAIN_IDENTIFIER = re.compile(_RUST_IDENTIFIER)
_BLOCK_COMMENT_OPEN = re.compile(r"^ {0,3}<!--")


def _error(path: Path, line_number: int, message: str) -> SystemExit:
    return SystemExit(f"ERROR: --goal {path}:{line_number}: {message}")


def _strip_closed_inline_comments(path: Path, line_number: int, line: str) -> str:
    """Remove same-line HTML comments, which are invisible heading contents.

    A multiline inline comment is legal CommonMark but needs block-container
    context to decide whether a later ATX marker is live. Refuse that uncommon
    ambiguity instead of silently disagreeing with the renderer.
    """

    output = []
    cursor = 0
    while True:
        start = line.find("<!--", cursor)
        if start < 0:
            output.append(line[cursor:])
            return "".join(output)
        output.append(line[cursor:start])
        end = line.find("-->", start + 4)
        if end < 0:
            raise _error(
                path,
                line_number,
                "multiline inline HTML comment is unsupported near prose target "
                "headings; put the comment on its own line",
            )
        cursor = end + 3


def parse_goal(path: Path) -> dict[str, object]:
    raw = path.read_bytes()
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise SystemExit(
            f"ERROR: --goal {path} is not valid UTF-8: byte {error.start}: "
            f"{error.reason}"
        ) from error

    targets: list[dict[str, object]] = []
    seen: dict[str, int] = {}
    unquoted_headings: list[dict[str, object]] = []
    fence: tuple[str, int] | None = None
    html_comment = False

    for line_number, raw_line in enumerate(text.splitlines(), 1):
        if html_comment:
            if "-->" in raw_line:
                html_comment = False
            # A CommonMark comment block consumes its complete closing line.
            continue

        if fence is not None:
            marker, width = fence
            closing = re.compile(rf"^ {{0,3}}{re.escape(marker)}{{{width},}}[ \t]*$")
            if closing.fullmatch(raw_line):
                fence = None
            # In particular, ```rust is content, not a closing fence.
            continue

        fence_match = _FENCE.match(raw_line)
        if fence_match:
            marker, info = fence_match.groups()
            # CommonMark forbids backticks in a backtick fence's info string.
            if marker[0] == "~" or "`" not in info:
                fence = (marker[0], len(marker))
                continue

        if _BLOCK_COMMENT_OPEN.match(raw_line):
            if "-->" not in raw_line[raw_line.find("<!--") + 4 :]:
                html_comment = True
            # Type-2 CommonMark HTML blocks consume the complete line, even
            # when their terminator also occurs on it.
            continue

        line = _strip_closed_inline_comments(path, line_number, raw_line)
        atx = _ATX.fullmatch(line)
        if atx:
            level = len(atx.group(1))
            content = (atx.group(2) or "").strip()
            # An optional whitespace-separated run of hashes closes an ATX
            # heading and is not part of its rendered contents.
            content = re.sub(r"[ \t]+#+[ \t]*$", "", content).strip()
            target = _TARGET.fullmatch(content)
            if level == 3 and target:
                name = target.group("name")
                resolution = target.group("resolution") or "decide"
                if name in seen:
                    raise SystemExit(
                        f"ERROR: --goal {path} repeats target heading {name!r} "
                        f"on lines {seen[name]} and {line_number}"
                    )
                seen[name] = line_number
                targets.append(
                    {
                        "name": name,
                        "resolution": resolution,
                        "line": line_number,
                    }
                )
            elif content.startswith("`"):
                raise _error(
                    path,
                    line_number,
                    f"malformed target heading {raw_line!r}; expected exactly "
                    "'### `function`', optionally followed by the exact marker "
                    "'[decide]' or '[prove]'",
                )
            elif level == 3 and _PLAIN_IDENTIFIER.fullmatch(content):
                # Ordinary H3 prose remains legal. After extraction, setup
                # rejects only the ambiguous case where this text is actually
                # an extracted function name and was likely meant as a target.
                unquoted_headings.append({"name": content, "line": line_number})
            continue

        # Nested/container headings need a full CommonMark block parser to map
        # safely onto the top-level target convention. A target-looking form is
        # therefore loud, never silently omitted.
        if re.search(r"#{1,6}[ \t]+`", line):
            raise _error(
                path,
                line_number,
                f"target-like text is not a supported top-level ATX heading: "
                f"{raw_line!r}",
            )

    if not targets:
        raise SystemExit(
            f"ERROR: --goal {path} declares no targets; add one or more exact "
            "'### `function`' headings"
        )
    for index, left_target in enumerate(targets):
        left = left_target["name"]
        for right_target in targets[index + 1 :]:
            right = right_target["name"]
            if left.startswith(right) or right.startswith(left):
                raise SystemExit(
                    f"ERROR: --goal {path} has prefix-ambiguous targets {left!r} "
                    f"and {right!r}; prose target names must be mutually prefix-free"
                )

    return {"targets": targets, "unquoted_headings": unquoted_headings}


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: pv_prose_goal.py <GOAL.md>")
    print(
        json.dumps(
            parse_goal(Path(sys.argv[1])),
            ensure_ascii=True,
            separators=(",", ":"),
        )
    )


if __name__ == "__main__":
    main()
