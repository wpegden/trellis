"""Keys for Claude's explicit folder-trust choice, when the pane is unambiguous."""

from __future__ import annotations

import re
from typing import Optional, Tuple


_ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
_CHOICE = re.compile(
    r"^\s*(?P<selected>[❯>])?\s*[12]\.\s*"
    r"(?P<label>Yes, I trust this folder|No, exit)\s*$"
)
_QUESTIONS = (
    "Is this a project you created or one you trust?",
    "Do you trust the files in this folder",
)


def is_folder_trust_screen(screen: str) -> bool:
    """Recognize only the two known folder-trust question texts."""
    plain = " ".join(_ANSI.sub("", screen).split())
    return any(question in plain for question in _QUESTIONS)


def claude_folder_trust_keys(screen: str) -> Optional[Tuple[str, ...]]:
    """Return keys only for the known dialog with both choices and a selection.

    Claude has shipped both Yes-first and No-first layouts. The cursor, rather
    than the option number, determines whether navigation is necessary.
    """
    plain = _ANSI.sub("", screen)
    if not is_folder_trust_screen(screen):
        return None
    choices = []
    for line in plain.splitlines():
        match = _CHOICE.fullmatch(line)
        if match:
            choices.append((match.group("label"), bool(match.group("selected"))))
    if len(choices) != 2 or {label for label, _ in choices} != {
        "Yes, I trust this folder", "No, exit"
    } or sum(selected for _, selected in choices) != 1:
        return None
    yes_at = next(i for i, (label, _) in enumerate(choices) if label == "Yes, I trust this folder")
    selected_at = next(i for i, (_, selected) in enumerate(choices) if selected)
    if selected_at == yes_at:
        return ("Enter",)
    return ("Down" if yes_at > selected_at else "Up", "Enter")
