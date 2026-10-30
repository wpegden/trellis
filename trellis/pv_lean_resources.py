"""Shared elaboration settings for Aeneas output."""
from __future__ import annotations

import re


_ARRAY_SIZE = re.compile(r"Array\.make\s+(\d+)#usize")
_LARGE_ARRAY_THRESHOLD = 256
_LARGE_ARRAY_RECDEPTH = 65536
_PREAMBLE_END = re.compile(
    r"^\s*(?:/--|namespace\b|@\[|(?:noncomputable\s+)?"
    r"(?:def|theorem|lemma|axiom|opaque|structure|inductive|class|instance)\b)"
)


def _preamble_end(lines: list[str]) -> int:
    return next((i for i, line in enumerate(lines) if _PREAMBLE_END.match(line)), len(lines))


def generated_elaboration_options(text: str) -> dict[str, int | bool]:
    """Expose missing model names instead of silently binding implicit parameters."""
    options: dict[str, int | bool] = {"autoImplicit": False}
    size = max((int(match[1]) for match in _ARRAY_SIZE.finditer(text)), default=0)
    if size < _LARGE_ARRAY_THRESHOLD:
        return options
    # Array.make's length side goal consumes recursion proportional to the
    # literal's size. Preserve a larger explicit setting in the free preamble.
    lines = text.splitlines()
    preamble = "\n".join(lines[:_preamble_end(lines)])
    configured = re.findall(r"(?m)^\s*set_option maxRecDepth (\d+)\s*$", preamble)
    depth = max(_LARGE_ARRAY_RECDEPTH, int(configured[-1]) if configured else 0)
    return {**options, "maxRecDepth": depth}


def prepare_generated_lean(text: str, *, imports: tuple[str, ...] | list[str] = ()) -> str:
    """Apply shared options to a checked copy, keeping declarations unchanged."""
    options = generated_elaboration_options(text)
    if not options and not imports:
        return text
    lines = text.splitlines(keepends=True)
    present = set(re.findall(r"(?m)^\s*import\s+(\S+)", text))
    added = [f"import {name}\n" for name in imports if name not in present]
    if added:
        last_import = max((i + 1 for i, line in enumerate(lines) if re.match(r"^\s*import\s+", line)), default=0)
        lines[last_import:last_import] = added
    end = _preamble_end(lines)
    for name, value in options.items():
        literal = str(value).lower()
        pattern = re.compile(rf"^(\s*set_option {name} )(?:\d+|true|false)(\s*)$")
        existing = [i for i in range(end) if pattern.match(lines[i])]
        if existing:
            i = existing[-1]
            lines[i] = pattern.sub(lambda m: f"{m[1]}{literal}{m[2]}", lines[i])
        else:
            lines.insert(end, f"set_option {name} {literal}\n")
            end += 1
    return "".join(lines)
