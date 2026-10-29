"""Declaration heads in generated Lean, with original source locations."""
from dataclasses import dataclass
import re


@dataclass(frozen=True)
class DeclarationHead:
    kind: str
    name: str
    line: int


def mask_comments(text: str, *, strings: bool = False) -> str:
    """Mask nested Lean comments without changing offsets or line numbers."""
    out = list(text)
    i, depth = 0, 0
    quoted = False
    while i < len(text):
        pair = text[i:i + 2]
        width, hide = 1, False
        if depth:
            hide = True
            if pair == "/-":
                depth += 1
                width = 2
            elif pair == "-/":
                depth -= 1
                width = 2
        elif quoted:
            hide = strings
            if text[i] == "\\":
                width = min(2, len(text) - i)
            elif text[i] == '"':
                quoted = False
        elif text[i] == "«":
            end = text.find("»", i + 1)
            width = (len(text) if end < 0 else end + 1) - i
        elif text[i] == "'" and (char := re.match(r"'(?:\\(?:u\{[0-9a-fA-F]+\}|.)|[^'\\])'", text[i:])):
            width = len(char[0])
        elif text[i] == '"':
            quoted, hide = True, strings
        elif pair == "/-":
            depth, width, hide = 1, 2, True
        elif pair == "--":
            end = text.find("\n", i)
            width, hide = (len(text) if end < 0 else end) - i, True
        if hide:
            for j in range(i, i + width):
                if out[j] != "\n":
                    out[j] = " "
        i += width
    return "".join(out)


_HEAD = re.compile(
    r"^[ \t]*(?:@\[[^\]]*\]\s*)*"
    r"(?:(?:noncomputable|partial|unsafe|private|protected|public)\s+)*"
    r"(?P<kind>def|impl_def|abbrev|theorem|lemma|inductive|structure|class|instance|axiom|opaque)"
    r"\s+(?P<name>(?:«[^»]*»|[^\s(:{«])+)",
    re.MULTILINE
)


def declaration_heads(text: str) -> list[DeclarationHead]:
    """Read names across whitespace; generated declaration bodies stay untouched."""
    code = mask_comments(text, strings=True)
    return [DeclarationHead(match["kind"], match["name"], code.count("\n", 0, match.start()) + 1)
            for match in _HEAD.finditer(code)]
