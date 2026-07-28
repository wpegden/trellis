"""Sidecar model driver (E§3) — raw-API, structurally body-frozen.

The only-BODY guarantee is STRUCTURAL: the driver alone writes the
node file, and it only ever splices model output below the byte-frozen
prefix (everything through the ``-- BODY`` line). The model cannot
edit imports, the statement, other nodes, or create files, because no
code path writes anything else.

Model interface: OpenAI-compatible chat completions (Leanstral first;
endpoint/model/key-path all config). The compile step is presented as
the model's native ``lean_run_code`` tool convention (E-D3), with a
fenced-code-block fallback mode for non-tool-calling models.

KEY HYGIENE (amendment A9): the Authorization header is constructed
inside ``ModelClient._post`` and NOWHERE else; request headers are
NEVER logged, and neither the key nor any header ever reaches the
ledger, the spool record, daemon.log, or an exception message.
``tests/test_sidecar_driver.py::test_key_never_leaks`` pins this.

Dependencies: stdlib + ``requests`` only.
"""

from __future__ import annotations

import json
import re
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Sequence, Tuple

from trellis.config import FORBIDDEN_KEYWORDS_DEFAULT
from trellis.sidecar.config import SidecarConfig

DRIVER_VERSION = "sidecar-driver-v3"

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
)

BANNED_TOKENS: Tuple[str, ...] = tuple(FORBIDDEN_KEYWORDS_DEFAULT) + (
    SIDECAR_EXTRA_BANNED_TOKENS
)

BODY_MARKER = "-- BODY"

# The four-axiom canonical floor (kernel CANONICAL_APPROVED_AXIOMS).
AXIOM_FLOOR: Tuple[str, ...] = (
    "propext",
    "funext",
    "Classical.choice",
    "Quot.sound",
)


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


# ---------------------------------------------------------------------------
# Prompt assembly (E§3.3 spec + amendment A3 tokens; owner-reviewed text)
# ---------------------------------------------------------------------------

SYSTEM_PROMPT_TEMPLATE = """You are a Lean 4 proof engineer working on one theorem in a locked project.

TASK: prove the theorem below by supplying ONLY the proof body — the code that
goes below the `-- BODY` marker. Everything above the marker (imports, options,
the statement) is frozen and will be restored if you attempt to change it.

RULES (violations make the attempt worthless):
- These tokens may not appear anywhere in your body text, comments included
  (a text scan rejects them): {banned_tokens}.
- The proof must close using only the approved axioms: {approved_axioms}.
  Anything else is rejected by the acceptance gate.
- Work with the imports already in scope. The body is the proof block itself,
  so every helper lives INSIDE it as a `have`/`let`/`suffices`/`obtain` step —
  a top-level declaration cannot go there. Give helpers ordinary mathematical
  names (the acceptance gate rejects authored names shaped like compiler
  internals: `_foo`, `eq_1`, `match_2`, `proof_3`, `omega_4`).
- A spot heartbeat raise `set_option maxHeartbeats N in <tactic>` inside the
  body is allowed, N <= 800000.
- Available: mathlib (pinned snapshot) and the project's Tablet.* lemmas whose
  statements are listed below. Cite Tablet lemmas by their principal name.

THE FILE (frozen prefix, then the marker; your body replaces everything after):
{file_prefix}

DEPENDENCY STATEMENTS in scope (principal declarations of imported Tablet nodes):
{dep_statements}

Use the lean_run_code tool to compile candidate bodies. Iterate on compiler
feedback until it compiles cleanly. When the compiler reports success, stop.
"""

LEAN_RUN_CODE_TOOL = {
    "type": "function",
    "function": {
        "name": "lean_run_code",
        "description": (
            "Submit a candidate proof body for the frozen theorem statement. "
            "The statement, imports and everything above the `-- BODY` marker "
            "are fixed; supply only the code that goes below the marker. It "
            "is compiled with lake/lean and full compiler output is returned."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "body": {
                    "type": "string",
                    "description": "Lean 4 code to place below -- BODY",
                }
            },
            "required": ["body"],
        },
    },
}

# ---------------------------------------------------------------------------
# v2 info tools (owner-directed; grunt-bench validated): on-disk NL
# proof access + loogle search. Info tools cost a turn (wall/token
# budgets apply), never a compile iteration — and their outputs are
# tool turns, never candidate bodies, so the ban scan does not apply
# to them (it guards only what reaches the compiler / the record).
#
# v4 RETRIEVAL WIDENING (SOL_VS_GRUNT_COMPARISON §4, owner-approved):
# the measured gap against a shell-equipped frontier model was
# retrieval, not reasoning — both of its decisive wins came from
# ripgrep over sibling `Tablet/*.lean` files the node does NOT import,
# plus reading mathlib source. So `read_file` now reaches every
# `Tablet/*.{lean,tex}` and mathlib source, and two grep tools
# (`search_tablet`, `search_mathlib_source`) supply the mechanism that
# actually won: SEARCH. All of it is READ access — the body-only write
# lock is untouched, the driver still splices only below `-- BODY` of
# its own node.
# ---------------------------------------------------------------------------

READ_FILE_MAX_BYTES = 32_768
# Windowed reads (a mathlib file is routinely larger than the byte cap,
# and the interesting proof is rarely at its top).
READ_FILE_MAX_WINDOW_LINES = 400
SEARCH_MAX_HITS = 8  # loogle
SEARCH_TIMEOUT_SECS = 90.0
# grep tools: total hits, per-file hits (one crowded file must not eat
# the whole budget), per-line and per-result byte truncation.
SEARCH_TABLET_MAX_HITS = 40
SEARCH_MATHLIB_MAX_HITS = 30
SEARCH_MAX_HITS_PER_FILE = 5
SEARCH_MAX_LINE_CHARS = 240
SEARCH_RESULT_MAX_BYTES = 24_000
SEARCH_DEFAULT_CONTEXT = 2
SEARCH_MAX_CONTEXT = 6
SEARCH_MAX_FILE_BYTES = 2_000_000
SEARCH_MIN_QUERY_CHARS = 3
# Hard scan guards. Both are ripgrep subprocess timeouts, so they are
# ENFORCED by killing the child rather than checked between files — the
# model controls the pattern, and no in-process regex deadline can
# interrupt a match already running (see `_ripgrep_hits`). The tablet is
# a few hundred small files; mathlib is ~8k files / ~110 MB, so its scan
# also carries a per-attempt call cap — a grunt burning its wall on
# greps would be a regression.
SEARCH_TABLET_DEADLINE_SECS = 10.0
SEARCH_MATHLIB_DEADLINE_SECS = 25.0
SEARCH_MATHLIB_MAX_CALLS = 40
# The workspace's pinned mathlib checkout (lake package layout).
MATHLIB_PACKAGE_REL = ".lake/packages/mathlib"
# The host Loogle precedent (scripts/loogle_json.sh): local server,
# fixed JSON endpoint.
LOOGLE_JSON_URL = "http://127.0.0.1:8088/json"

READ_FILE_TOOL = {
    "type": "function",
    "function": {
        "name": "read_file",
        "description": (
            "Read a project or mathlib source file. Readable paths: "
            "Tablet/<Name>.tex (the paper's statement and prose proof of a "
            "node), Tablet/<Name>.lean (that node's Lean proof — every "
            "already-closed sibling shows the house idiom for this project, "
            "and any node in the tablet is readable whether or not this node "
            "imports it), and mathlib source files under "
            ".lake/packages/mathlib/ as returned by search_mathlib_source. "
            "Pass start_line/max_lines to read a window of a long file."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": (
                        "e.g. Tablet/AlphaCapsDisjoint.tex, "
                        "Tablet/K33LineGraphBasic.lean, or "
                        ".lake/packages/mathlib/Mathlib/Combinatorics/"
                        "SimpleGraph/Coloring.lean"
                    ),
                },
                "start_line": {
                    "type": "integer",
                    "description": (
                        "1-based first line of the window. Omit to read "
                        "from the start of the file."
                    ),
                },
                "max_lines": {
                    "type": "integer",
                    "description": (
                        "Number of lines to return from start_line "
                        f"(at most {READ_FILE_MAX_WINDOW_LINES})."
                    ),
                },
            },
            "required": ["path"],
        },
    },
}

SEARCH_TABLET_TOOL = {
    "type": "function",
    "function": {
        "name": "search_tablet",
        "description": (
            "Search every file of this project's tablet — Tablet/*.lean "
            "(Lean proofs of nodes already closed) and Tablet/*.tex (the "
            "paper's prose) — for a regex or literal string, returning "
            "path:line with surrounding lines. The tablet is one project in "
            "one house style, so a tactic, a mathlib lemma or a definition "
            "you are about to use has usually been used already in a sibling "
            "node: search for it here first and reuse the working idiom, "
            "including from nodes this one does not import. Good queries are "
            "a definition name from your statement, a mathlib lemma name, or "
            "a tactic you are unsure how to drive."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": (
                        "Regex (ripgrep syntax: no lookaround, no "
                        "backreferences), or a plain substring — a pattern "
                        "that does not parse as a regex is matched "
                        "literally. e.g. `bicoloring`, `Coloring.mk`, "
                        "`map_rel_iff'`"
                    ),
                },
                "extension": {
                    "type": "string",
                    "enum": ["lean", "tex", "both"],
                    "description": (
                        "Which tablet files to search. Default both."
                    ),
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": (
                        "Default: case-sensitive when the query contains an "
                        "uppercase letter, case-insensitive otherwise."
                    ),
                },
                "context": {
                    "type": "integer",
                    "description": (
                        "Lines of surrounding context per hit, 0 to "
                        f"{SEARCH_MAX_CONTEXT} (default "
                        f"{SEARCH_DEFAULT_CONTEXT})."
                    ),
                },
            },
            "required": ["query"],
        },
    },
}

SEARCH_MATHLIB_SOURCE_TOOL = {
    "type": "function",
    "function": {
        "name": "search_mathlib_source",
        "description": (
            "Search the pinned mathlib SOURCE tree for a regex or literal "
            "string, returning path:line with surrounding lines. This shows "
            "how a lemma or definition is stated, what its hypotheses are "
            "named, and how mathlib's own proofs use it — the working call "
            "site, alongside the name and type that search_mathlib returns. "
            "Follow a hit with read_file on the reported path and line to "
            "see the whole proof. Narrow broad queries with path_filter."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": (
                        "Regex (ripgrep syntax: no lookaround, no "
                        "backreferences) or plain substring, "
                        "at least 3 characters. e.g. "
                        "`theorem chromaticNumber_le`, `Finite.card_subtype_lt`"
                    ),
                },
                "path_filter": {
                    "type": "string",
                    "description": (
                        "Substring the file path must contain, e.g. "
                        "`Combinatorics/SimpleGraph`. Use it whenever the "
                        "query alone is broad."
                    ),
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": (
                        "Default: case-sensitive when the query contains an "
                        "uppercase letter, case-insensitive otherwise."
                    ),
                },
                "context": {
                    "type": "integer",
                    "description": (
                        "Lines of surrounding context per hit, 0 to "
                        f"{SEARCH_MAX_CONTEXT} (default "
                        f"{SEARCH_DEFAULT_CONTEXT})."
                    ),
                },
            },
            "required": ["query"],
        },
    },
}

SEARCH_MATHLIB_TOOL = {
    "type": "function",
    "function": {
        "name": "search_mathlib",
        "description": (
            "Search mathlib for lemmas by name substring, constant, or type "
            "shape (Loogle syntax: e.g. `Finset.sum_le_sum`, `Real.exp`, "
            "`|- _ * _ <= _ * _`, `tsum, Filter.Tendsto`). Returns matching "
            "declaration names, types and modules. Use this instead of "
            "guessing lemma names."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Loogle query"}
            },
            "required": ["query"],
        },
    },
}

GET_GOALS_TOOL = {
    "type": "function",
    "function": {
        "name": "get_goals",
        "description": (
            "Inspect the Lean proof goal state (the ⊢ goals) at a "
            "position in your MOST RECENTLY CHECKED proof body (the "
            "last lean_run_code submission, or the initial `sorry` "
            "body before any submission). Use it to see exactly what "
            "remains to be proved instead of guessing from error text."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "line": {
                    "type": "integer",
                    "description": (
                        "1-based line number WITHIN the proof body "
                        "(line 1 = first line below -- BODY). Omit to "
                        "use the first line containing `sorry`, else "
                        "the last body line."
                    ),
                },
                "column": {
                    "type": "integer",
                    "description": (
                        "0-based column on that line. Omit for end of "
                        "line."
                    ),
                },
            },
            "required": [],
        },
    },
}

# ---------------------------------------------------------------------------
# Read allowlist + grep engine (v4 retrieval widening)
# ---------------------------------------------------------------------------

_TABLET_STEM_RE = re.compile(r"^[A-Za-z0-9_'-]+$")


def mathlib_source_root(repo: Path) -> Optional[Path]:
    """The workspace's mathlib source root, or None when the package is
    absent (the tool is then never advertised)."""
    package = Path(repo) / MATHLIB_PACKAGE_REL
    inner = package / "Mathlib"
    if inner.is_dir():
        return inner
    if package.is_dir():
        return package
    return None


def normalize_rel_path(path: str) -> Optional[str]:
    """A repo-relative POSIX path with no traversal, or None.

    Absolute paths, `..` segments, `~`, and empty paths are rejected
    outright — the resolved-containment check below is the second gate
    (symlinks), this one is the syntactic gate."""
    text = str(path or "").strip().replace("\\", "/")
    if not text or text.startswith("/") or text.startswith("~"):
        return None
    segments = [seg for seg in text.split("/") if seg not in ("", ".")]
    if not segments or any(seg == ".." for seg in segments):
        return None
    return "/".join(segments)


def _within(child: Path, parent: Path) -> bool:
    try:
        return child.resolve().is_relative_to(parent.resolve())
    except (OSError, ValueError):
        return False


def resolve_readable(
    repo: Path, path: str, *, mathlib_root: Optional[Path] = None
) -> Optional[Path]:
    """The on-disk path a `read_file` request may open, or None.

    Readable: `Tablet/<Stem>.lean` and `Tablet/<Stem>.tex` for ANY node
    in the tablet (the v4 widening — the import closure was exactly the
    wrong restriction, since the winning idiom lives in siblings the
    node does not import), and `.lean` files under the workspace's
    mathlib package. Everything else, every other directory, and every
    symlink pointing out of those two roots is refused."""
    repo = Path(repo)
    rel = normalize_rel_path(path)
    if rel is None:
        return None
    segments = rel.split("/")
    if len(segments) == 2 and segments[0] == "Tablet":
        stem, _, ext = segments[1].rpartition(".")
        if ext in ("lean", "tex") and _TABLET_STEM_RE.match(stem or ""):
            candidate = repo / rel
            if _within(candidate, repo / "Tablet"):
                return candidate
        return None
    if mathlib_root is not None and rel.endswith(".lean"):
        candidate = repo / rel
        if _within(candidate, mathlib_root):
            return candidate
    return None


def _truncate_line(line: str) -> str:
    line = line.rstrip("\n").replace("\t", "    ")
    if len(line) > SEARCH_MAX_LINE_CHARS:
        return line[:SEARCH_MAX_LINE_CHARS] + " …[truncated]"
    return line


def effective_case_sensitive(query: str, case_sensitive: Optional[bool]) -> bool:
    """Smart case when the caller leaves ``case_sensitive`` unset:
    case-sensitive exactly when the query carries an uppercase letter."""
    if case_sensitive is None:
        return any(ch.isupper() for ch in query)
    return bool(case_sensitive)


def _format_hit(rel: str, lineno: int, lines: List[str], context: int) -> str:
    low = max(1, lineno - context)
    high = min(len(lines), lineno + context)
    out = [f"{rel}:{lineno}"]
    for number in range(low, high + 1):
        marker = ">" if number == lineno else " "
        out.append(f"{marker} {number:>5} | {_truncate_line(lines[number - 1])}")
    return "\n".join(out)


def _read_lines(root: Path, rel: str) -> Optional[List[str]]:
    """Lines of ``root/rel``, or None.

    P4 CONTAINMENT. Every file the grep tools open goes through here, and
    the resolved path must stay inside ``root`` — the same
    ``resolve_readable`` gate ``read_file`` uses. Search roots are walked
    by an external tool whose contents we do not control (a mathlib bump
    can plant a ``*.lean`` SYMLINK at any time), so containment is
    enforced at the open, not at the walk: a symlink pointing at
    ``/etc/passwd`` or at this user's ``~/.codex/auth.json`` is refused
    here no matter how the path was discovered."""
    path = root / rel
    if not _within(path, root):
        return None
    try:
        if path.stat().st_size > SEARCH_MAX_FILE_BYTES:
            return None
        return path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return None


@dataclass
class GrepResult:
    blocks: List[str] = field(default_factory=list)
    hits: int = 0
    files_scanned: int = 0
    capped: bool = False
    # The query did not parse as a regex and was matched literally.
    literal: bool = False
    error: str = ""

    def rendered(self) -> str:
        return "\n".join(self.blocks)


@dataclass
class RipgrepHits:
    """`(relative path, line number)` matches from one ripgrep run."""

    hits: List[Tuple[str, int]] = field(default_factory=list)
    # Files that returned MORE than the per-file cap (rendered as the
    # "… more matches in X" nudge, as the retired Python scanner did).
    crowded: List[str] = field(default_factory=list)
    # The query did not parse as a ripgrep regex and was re-run literally.
    literal: bool = False
    capped: bool = False
    # Non-empty ⇒ ripgrep could not answer; the caller reports this to
    # the model verbatim and NEVER falls back to an in-process scan.
    error: str = ""


def _ripgrep_hits(
    root: Path,
    query: str,
    *,
    globs: Sequence[str],
    case_sensitive: bool,
    path_filter: str = "",
    max_hits: int,
    timeout: float,
    max_depth: int = 0,
    sort_paths: bool = False,
) -> RipgrepHits:
    """Match ``query`` under ``root`` with ripgrep, or report why not.

    U1 — THE ONLY MATCHING ENGINE. Both grep tools route here; there is
    no in-process ``re`` scan behind it. The model controls ``query``
    verbatim, and Python's ``re`` is a BACKTRACKING engine with no
    interruptible timeout: ``([A-Za-z_.]+)+ :=`` — a pattern a Lean model
    might write innocently — costs seconds at 24 characters and doubles
    every two, so against a real 40-80 character Lean line a single
    ``pattern.search()`` call runs effectively forever. It cannot be
    interrupted from Python (the deadline in the retired scanner was
    checked BETWEEN files and so never fired mid-match), and the attempt
    runner is unsandboxed, so one such call burned a grunt slot and a
    full-priority core until an operator noticed.

    Ripgrep's default engine is a finite automaton: linear in the subject
    length, no catastrophic backtracking, and — decisively — it runs in a
    CHILD PROCESS, so ``subprocess.run(timeout=...)`` is a hard bound
    that is actually enforced by killing the child. ``--engine default``
    is passed explicitly so a lookaround pattern can never silently
    upgrade the run to the backtracking PCRE2 engine; such a pattern is a
    parse error, and a parse error re-runs the query as a FIXED STRING
    (also linear). Both branches together are held under ``timeout``.

    ``--no-config`` keeps ``RIPGREP_CONFIG_PATH`` from reintroducing
    ``--pcre2``; ``--no-ignore --hidden`` make the scan independent of
    gitignore rules, which would otherwise silently hide the whole
    ``.lake/packages/mathlib`` tree behind a dot-directory."""
    import shutil
    import subprocess

    binary = shutil.which("rg")
    if binary is None:
        return RipgrepHits(
            error=(
                "ripgrep (rg) is not installed or not on PATH, and this tool "
                "has no in-process fallback. Use read_file on a path you "
                "already know, or search_mathlib to find a declaration by "
                "type shape, and report this to the operator."
            )
        )

    base = [
        binary,
        "--line-number",
        "--no-heading",
        "--color",
        "never",
        "--no-messages",
        "--no-config",
        "--no-ignore",
        "--hidden",
        "--engine",
        "default",
        "--max-filesize",
        str(SEARCH_MAX_FILE_BYTES),
        # One more than the cap, so a file that is over it can be NAMED
        # as crowded rather than silently truncated.
        "--max-count",
        str(SEARCH_MAX_HITS_PER_FILE + 1),
    ]
    if max_depth > 0:
        base += ["--max-depth", str(max_depth)]
    if sort_paths:
        # `--sort path` makes the walk single-threaded, so it is worth it
        # only where the tree is small: the tablet is a few hundred files
        # and its hit order was alphabetical before, which is what a
        # capped result set needs to stay reproducible between calls.
        # Mathlib (~8k files) stays on the parallel walk.
        base += ["--sort", "path"]
    for glob in globs:
        base += ["--glob", glob]
    base.append("--case-sensitive" if case_sensitive else "--ignore-case")

    started = time.monotonic()

    def _run(literal: bool) -> Tuple[Optional[int], str, str]:
        remaining = timeout - (time.monotonic() - started)
        if remaining <= 0:
            return None, "", "timeout"
        command = list(base)
        if literal:
            command.append("--fixed-strings")
        # The pattern after `--` so it can never be read as a flag.
        command += ["--", query, str(root)]
        try:
            completed = subprocess.run(
                command,
                capture_output=True,
                text=True,
                timeout=remaining,
                check=False,
            )
        except subprocess.TimeoutExpired:
            return None, "", "timeout"
        except (OSError, subprocess.SubprocessError) as exc:
            return None, "", type(exc).__name__
        return completed.returncode, completed.stdout or "", completed.stderr or ""

    literal = False
    code, stdout, note = _run(literal=False)
    if code is not None and code not in (0, 1):
        # A pattern ripgrep will not parse (a stray bracket, or the
        # lookaround the default engine refuses). Matching it literally
        # is what the model almost always meant and is always linear.
        literal = True
        code, stdout, note = _run(literal=True)
    if code is None:
        if note == "timeout":
            return RipgrepHits(
                error=(
                    f"search timed out after {timeout:.0f}s and was stopped. "
                    "Narrow the query (a longer, more literal fragment) or "
                    "add a path_filter."
                )
            )
        return RipgrepHits(error=f"search could not run ({note}).")
    if code not in (0, 1):
        return RipgrepHits(
            error=(
                f"ripgrep refused this query: {_truncate_line(note.strip())}"
                if note.strip()
                else "ripgrep refused this query."
            )
        )

    out = RipgrepHits(literal=literal)
    per_file: Dict[str, int] = {}
    for line in stdout.splitlines():
        head, sep, rest = line.partition(":")
        if not sep:
            continue
        number, sep2, _text = rest.partition(":")
        if not sep2 or not number.isdigit():
            continue
        try:
            rel = Path(head).relative_to(root).as_posix()
        except ValueError:
            continue
        if path_filter and path_filter not in rel:
            continue
        seen = per_file.get(rel, 0) + 1
        per_file[rel] = seen
        if seen > SEARCH_MAX_HITS_PER_FILE:
            if rel not in out.crowded:
                out.crowded.append(rel)
            continue
        out.hits.append((rel, int(number)))
        if len(out.hits) >= max_hits:
            out.capped = True
            break
    return out


def _hit_blocks(
    root: Path,
    display: Callable[[str], str],
    found: RipgrepHits,
    context: int,
) -> GrepResult:
    """Turn ripgrep's ``(path, line)`` hits into formatted blocks,
    reading each hit file at most once and re-checking containment."""
    out = GrepResult(capped=found.capped, literal=found.literal, error=found.error)
    if found.error:
        return out
    cached: Dict[str, Optional[List[str]]] = {}
    total_bytes = 0
    for rel, lineno in found.hits:
        if total_bytes >= SEARCH_RESULT_MAX_BYTES:
            out.capped = True
            break
        if rel not in cached:
            cached[rel] = _read_lines(root, rel)
            out.files_scanned += 1
        lines = cached[rel]
        if lines is None or lineno > len(lines):
            continue
        block = _format_hit(display(rel), lineno, lines, context)
        out.blocks.append(block)
        out.hits += 1
        total_bytes += len(block)
    for rel in found.crowded:
        if any(rel == hit_rel for hit_rel, _ in found.hits):
            out.blocks.append(
                f"  … more matches in {display(rel)} (refine the query)"
            )
    return out


def _clamp_context(value: Any) -> int:
    try:
        context = int(value)
    except (TypeError, ValueError):
        return SEARCH_DEFAULT_CONTEXT
    return max(0, min(SEARCH_MAX_CONTEXT, context))


def _case_sensitive_arg(args: Dict[str, Any]) -> Optional[bool]:
    if "case_sensitive" not in args or args.get("case_sensitive") is None:
        return None
    return bool(args.get("case_sensitive"))


def make_info_tool_handlers(
    repo: Path,
    node: str,
    *,
    search_enabled: bool = True,
    mathlib_source_enabled: Optional[bool] = None,
) -> Dict[str, Callable[[Dict[str, Any]], str]]:
    """Handlers for the v2 info tools, hardened per the grunt-bench
    v2.1 findings:

    * repeat-call cache — identical info-tool calls are answered from a
      per-attempt cache with an escalating ``REPEAT CALL #k`` warning
      instead of re-running (observed live: 168 search calls, the same
      3 dead-end queries 47–48× each, every one a full API turn);
    * loogle unknown-identifier auto-retry — a bare name that is not an
      exact identifier re-runs as a quoted name-substring search (what
      the model meant), and residual loogle errors carry a one-line
      syntax primer.

    ``search_enabled=False`` (no loogle configured for this project)
    omits the loogle handler entirely — the tool is absent, never
    advertised-but-broken. ``mathlib_source_enabled`` defaults to
    "whenever the workspace actually carries a mathlib checkout", the
    same absent-or-working rule.

    v4 retrieval widening: ``read_file`` reaches every
    ``Tablet/*.{lean,tex}`` and mathlib source; ``search_tablet`` and
    ``search_mathlib_source`` grep those two trees. Every new tool goes
    through the SAME repeat-call cache, so a model cannot loop on
    greps."""
    repo = Path(repo)
    mathlib_root = mathlib_source_root(repo)
    if mathlib_source_enabled is False:
        mathlib_root = None
    _cache: Dict[Tuple[str, str], str] = {}
    _repeats: Dict[Tuple[str, str], int] = {}
    _mathlib_calls = [0]

    def _dedup(tool: str, key: str, compute: Callable[[], str]) -> str:
        cache_key = (tool, key)
        if cache_key in _cache:
            _repeats[cache_key] = _repeats.get(cache_key, 1) + 1
            return (
                f"REPEAT CALL #{_repeats[cache_key]} — identical {tool} call "
                "already answered below. Do not ask again: use this result, "
                "try a DIFFERENT query, or submit a candidate body with "
                "lean_run_code.\n---\n" + _cache[cache_key]
            )
        result = compute()
        _cache[cache_key] = result
        _repeats[cache_key] = 1
        return result

    def _read(path: str, start_line: int, max_lines: int) -> str:
        target = resolve_readable(repo, path, mathlib_root=mathlib_root)
        if target is None:
            allowed = (
                "Tablet/<Name>.lean, Tablet/<Name>.tex for any node in "
                "this tablet"
            )
            if mathlib_root is not None:
                allowed += f", and .lean files under {MATHLIB_PACKAGE_REL}/"
            return f"read_file: {path!r} is not readable. Readable: {allowed}."
        try:
            text = target.read_text(encoding="utf-8", errors="replace")
        except OSError as exc:
            return f"read_file: cannot read {path}: {type(exc).__name__}"
        if start_line <= 0 and max_lines <= 0:
            if len(text) > READ_FILE_MAX_BYTES:
                total = text.count("\n") + 1
                text = (
                    text[:READ_FILE_MAX_BYTES]
                    + f"\n...[truncated; the file has {total} lines — pass "
                    "start_line/max_lines to read further]"
                )
            return text
        lines = text.splitlines()
        first = max(1, start_line or 1)
        count = max_lines if max_lines > 0 else READ_FILE_MAX_WINDOW_LINES
        count = min(count, READ_FILE_MAX_WINDOW_LINES)
        window = lines[first - 1 : first - 1 + count]
        if not window:
            return (
                f"read_file: {path} has {len(lines)} lines; start_line "
                f"{first} is past the end"
            )
        numbered = [
            f"{first + offset:>5} | {_truncate_line(line)}"
            for offset, line in enumerate(window)
        ]
        header = (
            f"{path} lines {first}-{first + len(window) - 1} of {len(lines)}"
        )
        return header + "\n" + "\n".join(numbered)

    def read_file(args: Dict[str, Any]) -> str:
        path = str(args.get("path", "")).strip()
        try:
            start_line = int(args.get("start_line") or 0)
        except (TypeError, ValueError):
            start_line = 0
        try:
            max_lines = int(args.get("max_lines") or 0)
        except (TypeError, ValueError):
            max_lines = 0
        key = f"{path}|{start_line}|{max_lines}"
        return _dedup("read_file", key, lambda: _read(path, start_line, max_lines))

    def _tablet_globs(extension: str) -> List[str]:
        return {"lean": ["*.lean"], "tex": ["*.tex"]}.get(
            extension, ["*.lean", "*.tex"]
        )

    def _search_tablet(
        query: str, extension: str, case_sensitive: Optional[bool], context: int
    ) -> str:
        directory = repo / "Tablet"
        if not directory.is_dir():
            return "search_tablet: this workspace has no Tablet files."
        # `--max-depth 1` keeps the scan to `Tablet/*.{lean,tex}` exactly
        # as the retired directory listing did.
        found = _hit_blocks(
            directory,
            lambda rel: f"Tablet/{rel}",
            _ripgrep_hits(
                directory,
                query,
                globs=_tablet_globs(extension),
                case_sensitive=effective_case_sensitive(query, case_sensitive),
                max_hits=SEARCH_TABLET_MAX_HITS,
                timeout=SEARCH_TABLET_DEADLINE_SECS,
                max_depth=1,
                sort_paths=True,
            ),
            context,
        )
        if found.error:
            return f"search_tablet: {found.error}"
        note = " (matched literally)" if found.literal else ""
        if not found.hits:
            return (
                f"search_tablet: no match for {query!r}{note} in Tablet/. "
                "Try a shorter fragment of the name, or a different spelling."
            )
        head = f"{found.hits} hit(s) for {query!r}{note} in Tablet/:"
        tail = (
            "\n… result cap reached; narrow the query for the rest."
            if found.capped
            else ""
        )
        return head + "\n" + found.rendered() + tail

    def search_tablet(args: Dict[str, Any]) -> str:
        query = str(args.get("query", "")).strip()
        if not query:
            return "search_tablet: empty query"
        extension = str(args.get("extension", "both")).strip().lower()
        if extension not in ("lean", "tex", "both"):
            extension = "both"
        context = _clamp_context(args.get("context", SEARCH_DEFAULT_CONTEXT))
        case_sensitive = _case_sensitive_arg(args)
        key = f"{query}|{extension}|{case_sensitive}|{context}"
        return _dedup(
            "search_tablet",
            key,
            lambda: _search_tablet(query, extension, case_sensitive, context),
        )

    def _mathlib_display(root: Path, rel: str) -> str:
        """The repo-relative path a hit is reported under — the same
        string read_file accepts."""
        try:
            prefix = root.relative_to(repo).as_posix()
        except ValueError:
            prefix = MATHLIB_PACKAGE_REL
        return f"{prefix}/{rel}"

    def _search_mathlib_source(
        query: str, path_filter: str, case_sensitive: Optional[bool], context: int
    ) -> str:
        root = mathlib_root
        if root is None:
            return "search_mathlib_source: this workspace has no mathlib source."
        found = _hit_blocks(
            root,
            lambda rel: _mathlib_display(root, rel),
            _ripgrep_hits(
                root,
                query,
                globs=["*.lean"],
                case_sensitive=effective_case_sensitive(query, case_sensitive),
                path_filter=path_filter,
                max_hits=SEARCH_MATHLIB_MAX_HITS,
                timeout=SEARCH_MATHLIB_DEADLINE_SECS,
            ),
            context,
        )
        if found.error:
            return f"search_mathlib_source: {found.error}"
        note = " (matched literally)" if found.literal else ""
        scope = f" under paths containing {path_filter!r}" if path_filter else ""
        if not found.hits:
            return (
                f"search_mathlib_source: no match for {query!r}{note}"
                f"{scope}. Try a shorter fragment, drop path_filter, or use "
                "search_mathlib to find the declaration by type shape."
            )
        head = (
            f"{found.hits} hit(s) for {query!r}{note}{scope} in mathlib source:"
        )
        tail = (
            "\n… result cap reached; narrow the query or add a path_filter "
            "for the rest."
            if found.capped
            else ""
        )
        return head + "\n" + found.rendered() + tail

    def search_mathlib_source(args: Dict[str, Any]) -> str:
        query = str(args.get("query", "")).strip()
        if len(query) < SEARCH_MIN_QUERY_CHARS:
            return (
                "search_mathlib_source: give a query of at least "
                f"{SEARCH_MIN_QUERY_CHARS} characters — mathlib is ~8000 "
                "files, so a short pattern matches everywhere."
            )
        path_filter = str(args.get("path_filter", "") or "").strip().strip("/")
        context = _clamp_context(args.get("context", SEARCH_DEFAULT_CONTEXT))
        case_sensitive = _case_sensitive_arg(args)
        key = f"{query}|{path_filter}|{case_sensitive}|{context}"

        def compute() -> str:
            if _mathlib_calls[0] >= SEARCH_MATHLIB_MAX_CALLS:
                return (
                    "search_mathlib_source: this attempt has used its "
                    f"{SEARCH_MATHLIB_MAX_CALLS} mathlib source searches. "
                    "Work from what you have found and submit a candidate "
                    "body with lean_run_code."
                )
            _mathlib_calls[0] += 1
            return _search_mathlib_source(
                query, path_filter, case_sensitive, context
            )

        return _dedup("search_mathlib_source", key, compute)

    def _loogle_once(query: str) -> Tuple[Optional[Dict[str, Any]], str]:
        import urllib.parse
        import urllib.request

        url = LOOGLE_JSON_URL + "?q=" + urllib.parse.quote_plus(query)
        try:
            with urllib.request.urlopen(url, timeout=SEARCH_TIMEOUT_SECS) as resp:
                return json.loads(resp.read().decode("utf-8", "replace")), ""
        except Exception as exc:  # noqa: BLE001 — degrade to a tool message
            return None, type(exc).__name__

    def _format_hits(payload: Dict[str, Any]) -> Optional[str]:
        hits = (payload or {}).get("hits") or []
        if not hits:
            return None
        lines: List[str] = []
        for hit in hits[:SEARCH_MAX_HITS]:
            name = hit.get("name", "?")
            type_ = (hit.get("type") or "").replace("\n", " ")
            module = hit.get("module", "")
            lines.append(f"{name} : {type_}  [{module}]")
        more = len(hits) - SEARCH_MAX_HITS
        if more > 0:
            lines.append(f"... and {more} more (refine the query)")
        return "\n".join(lines)

    def _search(query: str) -> str:
        payload, err = _loogle_once(query)
        if payload is None:
            return (
                f"search_mathlib: loogle unavailable ({err}); do not retry "
                "this query, reason from your own knowledge instead"
            )
        error = payload.get("error") if isinstance(payload, dict) else None
        # v2.1: a bare name that isn't an exact identifier is loogle's
        # most common dead end ("unknown identifier 'isClosedMap'") and
        # models retry it verbatim forever. Auto-retry as a quoted
        # name-substring search, which is what they meant.
        if error and "unknown identifier" in str(error) and '"' not in query:
            payload2, _err2 = _loogle_once(f'"{query}"')
            if payload2 is not None and not payload2.get("error"):
                formatted = _format_hits(payload2)
                if formatted:
                    return (
                        f"(no exact identifier {query!r}; showing name-"
                        f'substring matches for "{query}")\n' + formatted
                    )
        if error:
            return (
                f"search_mathlib: {error}\nQuery syntax: exact name, "
                '"name substring", constant, or type shape like '
                "|- _ * _ <= _ * _ ."
            )
        formatted = _format_hits(payload)
        if formatted:
            return formatted
        suggestions = (payload or {}).get("suggestions") or []
        out = "search_mathlib: no hits."
        if suggestions:
            out += " Suggestions: " + ", ".join(str(s) for s in suggestions[:5])
        return out

    def search_mathlib(args: Dict[str, Any]) -> str:
        query = str(args.get("query", "")).strip()
        if not query:
            return "search_mathlib: empty query"
        return _dedup("search_mathlib", query, lambda: _search(query))

    handlers: Dict[str, Callable[[Dict[str, Any]], str]] = {
        "read_file": read_file,
        "search_tablet": search_tablet,
    }
    if search_enabled:
        handlers["search_mathlib"] = search_mathlib
    if mathlib_root is not None:
        handlers["search_mathlib_source"] = search_mathlib_source
    return handlers


def info_tool_specs(
    *,
    search_enabled: bool = True,
    goals_enabled: bool = True,
    mathlib_source_enabled: bool = True,
) -> List[Dict[str, Any]]:
    """The OpenAI tool specs matching ``make_info_tool_handlers`` plus
    the v3 ``get_goals`` tool (its handler is built from the compile
    loop by the caller — ``goals_enabled`` keeps the spec and handler
    in lockstep). ``mathlib_source_enabled`` mirrors the workspace's
    mathlib checkout, so the grep tool is advertised exactly when its
    handler exists."""
    specs: List[Dict[str, Any]] = [READ_FILE_TOOL, SEARCH_TABLET_TOOL]
    if search_enabled:
        specs.append(SEARCH_MATHLIB_TOOL)
    if mathlib_source_enabled:
        specs.append(SEARCH_MATHLIB_SOURCE_TOOL)
    if goals_enabled:
        specs.append(GET_GOALS_TOOL)
    return specs


def load_approved_axioms_list(repo: Path, node: str) -> List[str]:
    """Floor ∪ APPROVED_AXIOMS.json global + per-node (E-D6: templated
    per node, never hardcoded). Parse failure => floor only (the prompt
    is advisory; the kernel gate is authoritative and fail-closed)."""
    axioms = list(AXIOM_FLOOR)
    path = Path(repo) / "APPROVED_AXIOMS.json"
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return axioms
    if isinstance(data, dict):
        for extra in data.get("global", []) or []:
            if isinstance(extra, str) and extra not in axioms:
                axioms.append(extra)
        nodes = data.get("nodes", {}) or {}
        if isinstance(nodes, dict):
            for extra in nodes.get(node, []) or []:
                if isinstance(extra, str) and extra not in axioms:
                    axioms.append(extra)
    return axioms


_IMPORT_RE = re.compile(r"^\s*import\s+Tablet\.([A-Za-z0-9_']+)\s*$", re.MULTILINE)


def dep_statement_blocks(repo: Path, node_content: str) -> str:
    """For each `import Tablet.X` in the node file: X's signature
    region (tablet-marker line through the `-- BODY` line, marker
    excluded). Median 321 B/node measured, so even 20 deps ≈ 10 kB."""
    blocks: List[str] = []
    for match in _IMPORT_RE.finditer(node_content):
        dep = match.group(1)
        if dep == "Preamble":
            continue
        dep_path = Path(repo) / "Tablet" / f"{dep}.lean"
        try:
            dep_content = dep_path.read_text(encoding="utf-8")
            prefix, _ = split_body_marker(dep_content)
        except (OSError, ValueError):
            continue
        marker_idx = prefix.find("-- [TABLET NODE:")
        region = prefix[marker_idx:] if marker_idx >= 0 else prefix
        # Drop the trailing `-- BODY` marker line itself.
        region_lines = region.splitlines()
        if region_lines and region_lines[-1].strip() == BODY_MARKER:
            region_lines = region_lines[:-1]
        blocks.append("\n".join(region_lines).strip())
    return "\n\n".join(blocks) if blocks else "(none)"


def build_system_prompt(repo: Path, node: str, node_content: str) -> str:
    prefix, _ = split_body_marker(node_content)
    return SYSTEM_PROMPT_TEMPLATE.format(
        banned_tokens=", ".join(BANNED_TOKENS),
        approved_axioms=", ".join(load_approved_axioms_list(repo, node)),
        node=node,
        file_prefix=prefix.rstrip("\n"),
        dep_statements=dep_statement_blocks(repo, node_content),
    )


# Tool guidance (owner-directed design: the prose proof stays on disk
# and is read on demand — never inlined into the prompt). The steps are
# assembled and numbered to match the tools actually advertised, so the
# prompt describes exactly the surface the model has.
V4_PROSE_STEP = """\
Read the paper's prose proof of this theorem at Tablet/{node}.tex with the
read_file tool before proposing a body. It is the roadmap your Lean proof
follows; return to it whenever you are stuck. The paper's statement and prose
proof of each dependency is at Tablet/<DepName>.tex."""

V4_TABLET_SEARCH_STEP = """\
Search this project's own tablet with search_tablet before writing a step from
scratch. Its closed nodes live in Tablet/*.lean, and read_file opens every one
of them, whether or not this node imports it. The tablet is one project in one
house style, so a sibling node has usually already driven the definition, the
mathlib lemma or the tactic you need, and its working line is the shortest
route to a compiling proof. Search for a name out of your own statement, then
read the sibling .lean the hit points at and reuse the idiom you find there."""

V4_LOOGLE_STEP = """\
Find mathlib lemmas with search_mathlib (Loogle): it takes a name, a constant,
or a type shape, and answers with matching declaration names and types. Search
for a name whenever you are unsure it exists, before citing it in a body."""

V4_MATHLIB_SOURCE_STEP = """\
Read how mathlib itself states and uses a result with search_mathlib_source,
which greps the pinned mathlib source and reports path:line with context. Open
any reported path with read_file, passing start_line and max_lines to window a
long file. Give a path_filter such as `Combinatorics/SimpleGraph` to keep a
broad query focused."""

V4_COMPILE_STEP = """\
Submit candidate bodies with lean_run_code and iterate on the compiler
feedback. Your body is spliced inside the theorem's `by` block, so every line
of it is a tactic step."""


def _numbered_steps(steps: Sequence[str]) -> str:
    out: List[str] = []
    for index, step in enumerate(steps, start=1):
        lines = step.splitlines()
        head = f"{index}. {lines[0]}"
        rest = [f"   {line}" for line in lines[1:]]
        out.append("\n".join([head] + rest))
    return "\n".join(out)

# v3 long-horizon guidance (get_goals + persist-through-compaction).
V3_GUIDANCE = """\
Use the get_goals tool to inspect the proof goal state (the ⊢ goals) at a line
of your last-checked body — after a failed lean_run_code, call it at the line
where the proof goes wrong to see the exact remaining goals and hypotheses
instead of reconstructing them from the error text.

This is a LONG task: keep working. If the conversation is compacted (you will
see a compaction summary you wrote yourself), pick the work back up from your
summary and continue — do not restart from scratch or repeat approaches your
summary marks as failed.
"""


def build_system_prompt_v2(
    repo: Path,
    node: str,
    node_content: str,
    *,
    search_enabled: bool = True,
    goals_enabled: bool = True,
    mathlib_source_enabled: bool = True,
) -> str:
    """v1 prompt + the tool-workflow guidance matching the advertised
    info tools (loogle omitted when it is not configured, the mathlib
    grep omitted when the workspace has no mathlib checkout; the v3
    get_goals + compaction guidance appended when ``goals_enabled``)."""
    steps = [V4_PROSE_STEP.format(node=node), V4_TABLET_SEARCH_STEP]
    if search_enabled:
        steps.append(V4_LOOGLE_STEP)
    if mathlib_source_enabled:
        steps.append(V4_MATHLIB_SOURCE_STEP)
    steps.append(V4_COMPILE_STEP)
    prompt = (
        build_system_prompt(repo, node, node_content)
        + "\nTOOLS AND WORKFLOW (follow this order):\n"
        + _numbered_steps(steps)
        + "\n"
    )
    if goals_enabled:
        prompt += "\n" + V3_GUIDANCE
    return prompt


# ---------------------------------------------------------------------------
# Model client (raw requests; single-flight; generous backoff)
# ---------------------------------------------------------------------------


class ModelTransportError(RuntimeError):
    """A model request that yielded no usable reply.

    ``http_status`` is the status of the LAST request, set when the
    endpoint ANSWERED it with a failing one and left None when nothing
    came back at all (a stall, a reset, a malformed call).
    ``http_failures`` counts every failing reply the whole call
    collected, however it ended. That is the difference
    `_classify_transport` needs beyond the clock: a stall AT the wall is
    genuinely ambiguous, since the socket timeout is derived FROM the
    remaining wall and so a dead endpoint and an exhausted budget expire
    at the same instant — but a failing reply is the provider naming a
    fault, on a request that completed, which is evidence the wall did
    not end the call. The COUNT is what the classifier reads, because a
    provider that rate-limits and THEN goes dark surfaces the stall,
    with nothing else left to remember the storm by."""

    def __init__(
        self,
        message: str,
        *,
        http_status: Optional[int] = None,
        http_failures: int = 0,
    ) -> None:
        super().__init__(message)
        self.http_status = http_status
        self.http_failures = http_failures


# Transport exceptions worth an in-attempt retry (grunt-bench §3.3:
# the free tier stalls >600 s mid-response; a ReadTimeout used to
# abort the whole attempt, discarding up to ~1.3M tokens of accumulated
# context). Matched by class NAME across the MRO so requests'
# exception types are recognized without importing requests here (and
# fakes in tests work): covers requests.exceptions.ReadTimeout /
# ConnectTimeout / ConnectionError (reset-by-peer), the builtin
# ConnectionError family, and chunked/protocol tear-downs.
_TRANSPORT_RETRYABLE_EXC_NAMES = frozenset(
    {
        "ReadTimeout",
        "ReadTimeoutError",
        "ConnectTimeout",
        "ConnectionError",
        "ConnectionResetError",
        "BrokenPipeError",
        "ChunkedEncodingError",
        "ProtocolError",
        "IncompleteRead",
    }
)


def _transport_exc_retryable(exc: BaseException) -> bool:
    return any(
        cls.__name__ in _TRANSPORT_RETRYABLE_EXC_NAMES for cls in type(exc).__mro__
    )


# Phrases in a 4xx body that indict the request's ``reasoning_effort``
# field itself (Mistral's strict validator rejects unknown/unsupported
# top-level fields with wording of this shape).
_EFFORT_REJECTION_MARKERS = (
    "reasoning_effort",
    "unknown field",
    "unknown parameter",
    "unrecognized field",
    "unrecognized parameter",
    "unsupported parameter",
    "unsupported field",
    "extra inputs are not permitted",
    "extra fields not permitted",
    "additional properties are not allowed",
)


def _response_body_text(response: Any) -> str:
    """Best-effort text of an error response (never raises)."""
    try:
        payload = response.json()
    except Exception:  # noqa: BLE001 — non-JSON or unreadable body
        payload = None
    if payload is not None:
        try:
            return json.dumps(payload)
        except (TypeError, ValueError):
            return str(payload)
    text = getattr(response, "text", "")
    return text if isinstance(text, str) else ""


def _error_indicts_reasoning_effort(response: Any) -> bool:
    """Whether a 4xx actually blames the ``reasoning_effort`` field.

    Without this test ANY non-429 4xx permanently disabled effort for
    the client — including 400s that have nothing to do with it (an
    unanswered tool_call_id, an over-long context), silently downgrading
    the model out of its trained regime for the rest of the attempt."""
    body = _response_body_text(response).lower()
    return any(marker in body for marker in _EFFORT_REJECTION_MARKERS)


class ModelClient:
    """Thin OpenAI-compatible chat client. One in-flight request at a
    time by construction (synchronous). 429/5xx retried with
    exponential backoff honoring Retry-After clamped to
    ``RETRY_AFTER_CAP_SECONDS`` (a bogus header must not wedge the
    single-flight daemon), at most 3 retries and wall-budget-aware on
    every one of them like the transport ladder. Transport stalls
    (ReadTimeout / connection reset) retry the SAME request up to
    ``TRANSPORT_RETRIES`` times on fresh connections (each ``post`` is
    a new connection), backoff ``TRANSPORT_BACKOFF_SECONDS``,
    wall-budget-aware: a retry that cannot fit before the attempt
    deadline is not taken. Only after exhaustion does the exception
    surface — and per the F1 semantics the attempt then ends
    transport-class (suspension-counting, never the attempted-set)."""

    MAX_RETRIES = 3
    BACKOFF_START_SECONDS = 15.0
    RETRY_AFTER_CAP_SECONDS = 300.0
    MAX_READ_TIMEOUT_SECONDS = 600.0
    TRANSPORT_RETRIES = 2
    TRANSPORT_BACKOFF_SECONDS = (30.0, 60.0)

    def __init__(
        self,
        config: SidecarConfig,
        api_key: str,
        *,
        sleep: Callable[[float], None] = time.sleep,
        post: Optional[Callable[..., Any]] = None,
        now: Callable[[], float] = time.monotonic,
        log: Callable[[str], None] = lambda _m: None,
    ) -> None:
        self._config = config
        self._api_key = api_key
        self._sleep = sleep
        self._now = now
        self._log = log
        # v3: dropped for this client's lifetime once the endpoint 4xx's
        # a request that carried reasoning_effort.
        self.effort_disabled = False
        # Cumulative transport-stall retries (telemetry).
        self.transport_retries = 0
        if post is None:
            import requests

            post = requests.post
        self._post = post

    def _effort(self) -> str:
        effort = (self._config.reasoning_effort or "").strip()
        if self.effort_disabled or effort in ("", "none"):
            return ""
        return effort

    def chat(
        self,
        messages: Sequence[Dict[str, Any]],
        *,
        tools: Optional[Sequence[Dict[str, Any]]] = None,
        read_timeout: Optional[float] = None,
    ) -> Dict[str, Any]:
        """``read_timeout`` lets the caller derive each request's read
        timeout from its remaining wall budget (F4: a request must not
        overshoot the attempt wall by up to the full default timeout);
        it is capped at ``MAX_READ_TIMEOUT_SECONDS`` either way. The
        UNCAPPED value sets this call's deadline, and the call ends by it
        plus at most TWO 1 s timeout floors: each request's socket
        timeout is the budget still LEFT (capped, and floored at 1 s so
        it stays positive — urllib3 rejects a timeout <= 0), and BOTH
        retry ladders, transport and HTTP, decline a backoff that would
        not fit before the deadline. The floors are the whole of the
        slop, and they chain exactly once: a gated backoff can resume
        with a sub-second remainder, and if THAT floored request answers
        a 4xx indicting ``reasoning_effort`` inside its floor, the
        fallback — an immediate retry, no backoff to gate — starts past
        the deadline and is floored in its turn. Nothing chains a third,
        since the fallback fires once per client and every other retry
        is gated.

        v3: ``reasoning_effort`` (Leanstral trained regime) is sent as
        the top-level OpenAI-compat field when configured. Mistral
        validates it strictly (422 on unknown values), so a 4xx whose
        BODY indicts the field triggers ONE logged fallback retry
        without it, disabling it for this client's lifetime; a 4xx about
        anything else keeps the trained regime and raises normally."""
        payload: Dict[str, Any] = {
            "model": self._config.model_name,
            "messages": list(messages),
            "temperature": 1.0,
        }
        effort = self._effort()
        if effort:
            payload["reasoning_effort"] = effort
        if tools:
            payload["tools"] = list(tools)
        if read_timeout is None:
            read_timeout = self.MAX_READ_TIMEOUT_SECONDS
        # The wall proxy: remaining attempt wall (+ grace) as handed in.
        deadline = self._now() + max(1.0, float(read_timeout))
        delay = self.BACKOFF_START_SECONDS
        http_retries = 0
        http_failures = 0
        transport_failures = 0
        last_status = None
        while True:
            # Every request's socket timeout is re-derived from the
            # budget LEFT, so no request can outlive the deadline. Fixed
            # at the entry value it could not: admission only asks that
            # the backoff fit, so a retry taken at deadline-epsilon then
            # ran the full entry timeout, and a black-holed endpoint
            # overshot a 5400 s wall by 629 s against a documented grace
            # of 30.
            #
            # The 1 s floor keeps the timeout POSITIVE, and it is
            # load-bearing rather than defensive: urllib3 raises
            # ValueError for any timeout <= 0, which would replace the
            # real diagnosis with "model request failed: ValueError".
            # A gated backoff can still leave a sub-second remainder, and
            # the effort fallback retries with no backoff to gate at all,
            # after a 4xx that may itself have arrived at the deadline.
            # The two chain — a floored request can be the one that 4xx's
            # — so the true overshoot bound is wall + grace + (2 * floor
            # - eps), not wall + grace exactly.
            #
            # Declining to ADMIT a retry that could only get a near-floor
            # timeout was considered and rejected: against a live but slow
            # provider a 1 s retry can still return a small final answer,
            # and forfeiting that buys nothing the classification needs
            # (a declined retry leaves the failure below the wall, where
            # the clock already reads `error`).
            timeout = max(
                1.0, min(self.MAX_READ_TIMEOUT_SECONDS, deadline - self._now())
            )
            # A9 key hygiene: headers are built HERE, used for the call,
            # and never logged, stored, or attached to exceptions.
            headers = {
                "Authorization": f"Bearer {self._api_key}",
                "Content-Type": "application/json",
            }
            try:
                response = self._post(
                    self._config.endpoint,
                    headers=headers,
                    json=payload,
                    timeout=timeout,
                )
            except Exception as exc:  # transport-level
                if (
                    _transport_exc_retryable(exc)
                    and transport_failures < self.TRANSPORT_RETRIES
                ):
                    backoff = self.TRANSPORT_BACKOFF_SECONDS[
                        min(
                            transport_failures,
                            len(self.TRANSPORT_BACKOFF_SECONDS) - 1,
                        )
                    ]
                    if self._now() + backoff < deadline:
                        transport_failures += 1
                        self.transport_retries += 1
                        self._sleep(backoff)
                        continue
                raise ModelTransportError(
                    f"model request failed: {type(exc).__name__}",
                    http_failures=http_failures,
                )
            status = getattr(response, "status_code", 0)
            if status == 200:
                return response.json()
            last_status = status
            # Counted for EVERY failing reply, on the one path all of
            # them pass through, so a stall that ends the call still
            # carries the faults the provider named before it.
            http_failures += 1
            if (
                400 <= status < 500
                and status != 429
                and "reasoning_effort" in payload
                and _error_indicts_reasoning_effort(response)
            ):
                # Graceful ONCE-per-client fallback: a strict endpoint
                # (422/400) rejected the effort FIELD — established by
                # reading the error body, so a 4xx about anything else
                # leaves effort on and falls through to the normal error
                # path. Drop it and retry immediately (this retry
                # consumes no http-retry budget).
                self.effort_disabled = True
                del payload["reasoning_effort"]
                self._log(
                    f"reasoning_effort rejected (HTTP {status}); "
                    "falling back without it for this client"
                )
                continue
            if status == 429 or 500 <= status < 600:
                if http_retries >= self.MAX_RETRIES:
                    raise ModelTransportError(
                        f"model request exhausted retries (last HTTP {last_status})",
                        http_status=last_status,
                        http_failures=http_failures,
                    )
                retry_after = None
                try:
                    retry_after = float(response.headers.get("Retry-After", ""))
                except (TypeError, ValueError, AttributeError):
                    retry_after = None
                if retry_after is not None:
                    # Clamp: a bogus Retry-After (e.g. 86400) must not
                    # wedge the single-flight daemon.
                    retry_after = min(retry_after, self.RETRY_AFTER_CAP_SECONDS)
                backoff = retry_after if retry_after else delay
                # Deadline-gated exactly as the transport backoff below
                # is. Ungated, this was the one ladder that could outlive
                # the deadline the whole call is supposed to end by: a
                # rate-limit storm slept three CLAMPED Retry-Afters of
                # 300 s wherever it began, so one starting at 5399 s of a
                # 5400 s wall ran to 6303 s — 903 s past a documented
                # grace of 30.
                if self._now() + backoff < deadline:
                    http_retries += 1
                    self._sleep(backoff)
                    delay *= 2
                    continue
                raise ModelTransportError(
                    "model request has no budget left to retry "
                    f"(last HTTP {last_status})",
                    http_status=last_status,
                    http_failures=http_failures,
                )
            # The BODY is the whole diagnosis for a non-retryable 4xx —
            # a context-length overflow and a malformed history are
            # indistinguishable from the status alone (two live-run 400s
            # were unattributable for exactly this reason). Bounded, and
            # key-safe: headers are request-side and never echoed back.
            body = " ".join(_response_body_text(response).split())
            raise ModelTransportError(
                f"model request rejected: HTTP {status}"
                + (f": {body[:200]}" if body else ""),
                http_status=status,
                http_failures=http_failures,
            )


# ---------------------------------------------------------------------------
# Attempt loop
# ---------------------------------------------------------------------------


@dataclass
class CompileVerdict:
    ok: bool
    log: str


@dataclass
class AttemptResult:
    status: str  # success | failed | budget_exhausted | error
    proof_body: str = ""
    iterations: int = 0
    prompt_tokens: int = 0
    completion_tokens: int = 0
    wall_secs: float = 0.0
    detail: str = ""
    # Phase telemetry (grunt-bench fair-cost instrumentation):
    # cumulative model-turn and compile-callback wall time.
    api_secs: float = 0.0
    check_secs: float = 0.0
    info_tool_calls: int = 0
    # v4: calls per info-tool kind (read_file / search_tablet /
    # search_mathlib / search_mathlib_source / get_goals) — the measure
    # of whether the widened retrieval surface is used, and by which
    # tool, on attempts that close and attempts that do not.
    info_tool_counts: Dict[str, int] = field(default_factory=dict)
    # -- v3 long-regime telemetry ---------------------------------------
    compactions: int = 0
    # Cumulative (prompt+completion) total at the moment each compaction
    # fired — per-round deltas are first differences.
    compaction_round_tokens: List[int] = field(default_factory=list)
    transport_retries: int = 0
    reasoning_effort: str = ""  # effort actually in force at attempt end


_FENCE_RE = re.compile(r"```(?:lean)?\n(.*?)```", re.DOTALL)

# F4: per-request read-timeout grace beyond the remaining wall budget —
# a request issued near the wall may still return a small final answer.
WALL_TIMEOUT_GRACE_SECONDS = 30.0


# Stands in for the text of a reply that had none (see `_assistant_echo`).
ASSISTANT_NO_TEXT_PLACEHOLDER = "(no text in this reply)"


def _content_text(content: Any) -> str:
    """Normalize an assistant ``content`` to plain text.

    With ``reasoning_effort`` set, Mistral returns a chunk list —
    ``[{"type": "thinking", ...}, {"type": "text", "text": ...}]`` —
    instead of a string. Thinking chunks are DROPPED: they are never
    echoed back into the history (the uncached free tier re-bills the
    entire context every turn; the vibe harness likewise never re-sends
    reasoning traces)."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: List[str] = []
        for chunk in content:
            if isinstance(chunk, dict) and chunk.get("type") == "text":
                parts.append(str(chunk.get("text", "")))
        return "".join(parts)
    return ""


def _extract_tool_body(message: Dict[str, Any]) -> Optional[Tuple[str, str]]:
    """(tool_call_id, body) from an assistant message, or None."""
    for call in message.get("tool_calls") or []:
        function = call.get("function") or {}
        if function.get("name") == "lean_run_code":
            try:
                arguments = json.loads(function.get("arguments") or "{}")
            except ValueError:
                return (str(call.get("id", "")), "")
            return (str(call.get("id", "")), str(arguments.get("body", "")))
    return None


def _extract_tool_calls(
    message: Dict[str, Any],
) -> List[Tuple[str, str, Dict[str, Any]]]:
    """All (id, name, args) tool calls from an assistant message."""
    out: List[Tuple[str, str, Dict[str, Any]]] = []
    for call in message.get("tool_calls") or []:
        function = call.get("function") or {}
        name = str(function.get("name", ""))
        try:
            args = json.loads(function.get("arguments") or "{}")
            if not isinstance(args, dict):
                args = {}
        except ValueError:
            args = {}
        out.append((str(call.get("id", "")), name, args))
    return out


def _extract_fenced_body(message: Dict[str, Any]) -> Optional[str]:
    content = _content_text(message.get("content"))
    blocks = _FENCE_RE.findall(content)
    return blocks[-1] if blocks else None


# ---------------------------------------------------------------------------
# v3 context compaction (the Leanstral trained long-horizon regime).
#
# Mirrors the mistral-vibe harness Leanstral is trained in (source:
# vibe/core/compaction/* + the Leanstral report §2.2.2): trigger on the
# live context-token size (last turn's prompt+completion usage); ask the
# MODEL to write its own <summary>-wrapped handoff as a user message on
# top of the live conversation; then reset the history to
# [system prompt, envelope] where the envelope preserves the ORIGINAL
# task message verbatim alongside the compacted summary ("this reduces
# objective drift"). Our envelope additionally carries the latest
# compiler feedback — the single most load-bearing piece of loop state.
# Budgets are CUMULATIVE across compaction rounds.
# ---------------------------------------------------------------------------

COMPACTION_PROMPT = """CRITICAL: Respond with text only. Do NOT call any tools. Any tool call will be rejected.

You are performing a CONTEXT CHECKPOINT COMPACTION. The conversation will be
replaced by your summary and you will resume the SAME proof attempt from it.

Include:
- CURRENT BEST BODY: your most promising candidate proof body so far, complete
  and verbatim (Lean code), and what the compiler said about it
- LEARNINGS: lemma names that exist (with their signatures), goal states you
  inspected, facts taken from the paper proof — everything worth keeping
- FAILED ROUTES: approaches that failed and must NOT be retried, each with a
  one-line reason
- PLAN: the concrete next steps, in order

Be concise and structured. Do not repeat information across sections.

Wrap the ENTIRE summary in <summary></summary> tags and output nothing outside them:

<summary>
...your handoff summary here...
</summary>"""

COMPACTION_ENVELOPE = """You are continuing a proof attempt after a context compaction.

Here is the original task message, preserved verbatim. Treat it as prior context, not as a new request:

<previous_user_messages>
<previous_user_message>{original_task}</previous_user_message>
</previous_user_messages>

Here is the handoff summary you wrote before compaction (round {round}):

<compaction_summary>{summary}</compaction_summary>

Latest compiler feedback on your last submitted body:

{last_feedback}

Continue the attempt from your summary. Do not restart from scratch and do not retry routes your summary marks as failed."""

_SUMMARY_RE = re.compile(r"<summary>(.*?)</summary>", re.DOTALL)


def _extract_summary(text: str) -> str:
    match = _SUMMARY_RE.search(text or "")
    return (match.group(1) if match else (text or "")).strip()


def run_attempt(
    *,
    config: SidecarConfig,
    client: ModelClient,
    system_prompt: str,
    initial_body: str,
    compile_body: Callable[[str], CompileVerdict],
    now: Callable[[], float] = time.monotonic,
    info_tools: Optional[Dict[str, Callable[[Dict[str, Any]], str]]] = None,
    extra_tool_specs: Optional[Sequence[Dict[str, Any]]] = None,
    on_info_tool: Optional[Callable[[str, Dict[str, Any], str], None]] = None,
    on_compaction: Optional[Callable[[int, int, str], None]] = None,
) -> AttemptResult:
    """The E§3.4 loop. ``compile_body`` is the injected splice+compile
    callback (the compile_loop module in production; a fake in tests);
    it receives the candidate BODY only — the driver never hands the
    model or the callback anything above the marker to write.

    Budgets (wall-clock regime): wall (config.attempt_wall_seconds) is
    the primary and, by default, sole per-attempt budget. ``max_iterations``
    and ``attempt_tokens`` gate the loop only when POSITIVE — 0 means
    "no cap" (the wall alone ends the attempt). All budgets are
    CUMULATIVE across compaction rounds.

    v2 (owner-directed): ``info_tools`` maps extra tool names
    (read_file, search_mathlib, get_goals) to handlers; ``extra_tool_specs``
    are their OpenAI specs. Info-tool calls cost a turn (wall/token
    budgets apply) but NOT a compile iteration — compile iterations
    remain the count of lean_run_code submissions. Info-tool OUTPUTS are
    tool turns, never candidate bodies: the ban scan keeps applying only
    to lean_run_code bodies. v1 behavior is byte-identical when no info
    tools are passed.

    v3: when ``config.compact_context_tokens`` > 0 and the live context
    (last turn's prompt+completion usage) reaches it, the model writes
    its own <summary> handoff and the history is reset to
    [system prompt, envelope(original task + summary + last feedback)] —
    the trained vibe-harness compaction shape. ``on_compaction`` is
    called with (round, cumulative_tokens_at_compaction, summary).
    """
    started = now()
    info_tools = info_tools or {}
    original_task = (
        "Current body below the marker (replace it):\n```lean\n"
        + initial_body
        + "\n```\nCall lean_run_code with your candidate body."
    )
    messages: List[Dict[str, Any]] = [
        {"role": "system", "content": system_prompt},
        {"role": "user", "content": original_task},
    ]
    tools = (
        [LEAN_RUN_CODE_TOOL] + list(extra_tool_specs or [])
        if config.tool_calls
        else None
    )
    result = AttemptResult(status="failed")
    nudged = False
    last_context_tokens = 0
    last_feedback = "(no candidate submitted yet)"
    last_body = ""

    def _answer_info_calls(info_calls, unknown_calls, extra_lean_calls=()) -> None:
        """Answer EVERY tool call echoed back in the assistant message.

        The echo re-sends the assistant's whole ``tool_calls`` array, and
        a strict endpoint 400s the next request if any tool_call_id in it
        goes unanswered — which ends the attempt transport-class and, with
        the same prompt regenerating the same reply, respawns forever.
        Info calls are answered one result per id (the model may call
        read_file / search_mathlib / get_goals several times in a turn);
        extra lean_run_code calls beyond the compiled one get a "one
        compile per turn" result rather than a second compile, because a
        single turn has exactly one candidate body — feedback, iteration
        count and last_body all track that one submission."""
        for cid, name, args in info_calls:
            # P9: an info tool NEVER ends the attempt. Its arguments are
            # model-controlled, and a handler can raise well outside the
            # exception class it was written to expect — a deeply nested
            # query raises RecursionError from the regex compiler, a huge
            # repeat count raises OverflowError, and neither is an
            # `re.error`. Unhandled, they propagated out of the whole
            # attempt: up to 90 minutes of work and, per the ledger, up
            # to 150M prompt tokens discarded, the transport-suspension
            # counter ticked, and at ERROR_STREAK_THRESHOLD the node's
            # generation burned — all from one malformed search string.
            # A tool-level error message lets the model simply try
            # another query.
            try:
                output = info_tools[name](args)
            except Exception as exc:  # noqa: BLE001 — tool-level, never fatal
                output = (
                    f"{name}: this call failed ({type(exc).__name__}). The "
                    "arguments are probably malformed — try a simpler query."
                )
            result.info_tool_calls += 1
            result.info_tool_counts[name] = result.info_tool_counts.get(name, 0) + 1
            if on_info_tool is not None:
                on_info_tool(name, args, output)
            messages.append({"role": "tool", "tool_call_id": cid, "content": output})
        for cid, name, _args in unknown_calls:
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": cid,
                    "content": f"unknown tool {name}",
                }
            )
        for cid in extra_lean_calls:
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": cid,
                    "content": (
                        "NOT COMPILED: one lean_run_code call per turn — only "
                        "the first candidate body in a message is compiled. "
                        "Resubmit this body on its own turn if you still want "
                        "it checked."
                    ),
                }
            )

    def _governed_chat(msgs: List[Dict[str, Any]]) -> Dict[str, Any]:
        """One governed API turn: wall-derived read timeout, usage folded
        into the CUMULATIVE result totals, live context size tracked, and
        api_secs accrued even when the request raises."""
        nonlocal last_context_tokens
        remaining_wall = config.attempt_wall_seconds - (now() - started)
        api_started = now()
        try:
            reply = client.chat(
                msgs,
                tools=tools,
                read_timeout=remaining_wall + WALL_TIMEOUT_GRACE_SECONDS,
            )
        finally:
            result.api_secs += max(0.0, now() - api_started)
        usage = reply.get("usage") or {}
        prompt_toks = int(usage.get("prompt_tokens", 0) or 0)
        completion_toks = int(usage.get("completion_tokens", 0) or 0)
        result.prompt_tokens += prompt_toks
        result.completion_tokens += completion_toks
        last_context_tokens = prompt_toks + completion_toks
        return reply

    def _classify_transport(
        exc: ModelTransportError, retries_before: int, where: str = ""
    ) -> None:
        """F6: a transport failure that lands ON the wall deadline is the
        wall, not a fault. The read timeout is DERIVED from the remaining
        wall (+ grace), so an attempt that simply runs out of budget
        mid-request surfaces as ReadTimeout at wall+grace — and `error`
        is the transport class, which publishes no outcome, so the
        generation is never spent and the node is re-attempted from
        scratch. Classify by the clock (the success path below already
        does), keeping the transport cause in the detail.

        The clock ALONE is not enough. `ModelClient.chat` bounds a whole
        call by the deadline it derived on entry, so a black-holed
        endpoint spends its retries and backoffs INSIDE the budget and
        surfaces at exactly wall + grace — the same instant a clean
        exhaustion surfaces, having begun with 23 minutes of budget in
        hand. Reading that as `budget_exhausted` takes the NON-transport
        branch of `_bookkeep_outcome`, which RESETS
        `consecutive_transport_failures` and `error_streaks`: it does
        not merely fail to feed the transport circuit breaker, it
        disarms it. A clean wall exhaustion is the case where the
        wall-derived timeout fired first time, so require ZERO retries
        on THIS call — the DELTA, not the client's cumulative counter,
        since an earlier stall that recovered says nothing about how
        this call ended (one live misfiling was of exactly that shape).

        Nor is the retry delta alone enough, because it only counts the
        TRANSPORT ladder. A call that collected any failing REPLY is not
        a candidate for the wall at all, whatever the clock and whatever
        the delta: the endpoint ANSWERED, so the provider named a fault
        while this call was running, and a named fault is not a spent
        budget. A 429 storm is the case in point — it retries through a
        ladder the transport counter never sees.

        What the classifier reads is the COUNT of failing replies, not
        the last status, because the last status is only how the call
        ENDED. A storm that ends in a black hole (the provider rate-
        limits, then goes dark) surfaces as a bare stall: no status, no
        transport retry — the HTTP door, with the retry guard armed and
        looking the other way — and by then the count is the only thing
        left that remembers the provider was at fault. It also covers
        the one answered failure that takes no retry at all, the
        ``reasoning_effort`` fallback's 4xx.

        Residual band, accepted: a SILENT outage — no failing reply
        anywhere in the call, no room for a retry — lands past the wall
        exactly when its request entered with under 600 s of budget, so
        one beginning in the last 600 s still reads `budget_exhausted`.
        Under ~570 s the 600 s cap is not even reached and the
        observation is identical to a clean wall run — indistinguishable,
        not a heuristic gap."""
        prefix = f"{where}: " if where else ""
        retried = getattr(client, "transport_retries", 0) > retries_before
        answered = getattr(exc, "http_failures", 0) > 0
        if (
            not retried
            and not answered
            and now() - started > config.attempt_wall_seconds
        ):
            result.status = "budget_exhausted"
            result.detail = f"{prefix}wall budget (transport: {exc})"
        else:
            result.status = "error"
            result.detail = f"{prefix}{exc}"

    while True:
        # Budget gates. Iteration and token caps apply only when POSITIVE
        # (0 == disabled: the wall alone ends the attempt).
        if config.max_iterations > 0 and result.iterations >= config.max_iterations:
            result.status = "budget_exhausted"
            result.detail = "iteration budget"
            break
        if now() - started > config.attempt_wall_seconds:
            result.status = "budget_exhausted"
            result.detail = "wall budget"
            break
        if (
            config.attempt_tokens > 0
            and result.prompt_tokens + result.completion_tokens
            > config.attempt_tokens
        ):
            result.status = "budget_exhausted"
            result.detail = "token budget"
            break

        # -- v3 compaction gate (after budget checks: budgets win) -------
        if (
            config.compact_context_tokens > 0
            and last_context_tokens >= config.compact_context_tokens
        ):
            messages.append({"role": "user", "content": COMPACTION_PROMPT})
            retries_before = getattr(client, "transport_retries", 0)
            try:
                # Tools stay offered (strict endpoints can reject a
                # role:"tool" history without a tools param); a tool-call
                # reply is a fumbled summary, handled below.
                reply = _governed_chat(messages)
            except ModelTransportError as exc:
                _classify_transport(exc, retries_before, "compaction turn")
                break
            choices = reply.get("choices") or [{}]
            message = (choices[0] or {}).get("message") or {}
            summary = _extract_summary(_content_text(message.get("content")))
            if not summary:
                # Persist through compaction even when the model fumbles
                # the summary turn: synthesize the minimum viable handoff.
                summary = (
                    "(no summary was produced)\nLast submitted body:\n```lean\n"
                    + (last_body or initial_body)
                    + "\n```\nLast compiler feedback:\n"
                    + last_feedback
                )
            result.compactions += 1
            result.compaction_round_tokens.append(
                result.prompt_tokens + result.completion_tokens
            )
            if on_compaction is not None:
                on_compaction(
                    result.compactions,
                    result.prompt_tokens + result.completion_tokens,
                    summary,
                )
            envelope = COMPACTION_ENVELOPE.format(
                original_task=original_task,
                round=result.compactions,
                summary=summary,
                last_feedback=last_feedback,
            )
            messages = [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": envelope},
            ]
            last_context_tokens = 0
            continue

        retries_before = getattr(client, "transport_retries", 0)
        try:
            reply = _governed_chat(messages)
        except ModelTransportError as exc:
            _classify_transport(exc, retries_before)
            break
        # F4: re-check the wall right after the API returns so a request
        # that straddled the deadline terminates the attempt promptly
        # (no compile, no further turns).
        if now() - started > config.attempt_wall_seconds:
            result.status = "budget_exhausted"
            result.detail = "wall budget"
            break
        choices = reply.get("choices") or [{}]
        message = (choices[0] or {}).get("message") or {}

        calls = _extract_tool_calls(message) if config.tool_calls else []
        info_calls = [(i, n, a) for i, n, a in calls if n in info_tools]
        unknown_calls = [
            (i, n, a)
            for i, n, a in calls
            if n != "lean_run_code" and n not in info_tools
        ]
        has_lean_call = any(n == "lean_run_code" for _, n, _ in calls)

        if calls and not has_lean_call:
            # v2 info-only turn (read_file / search_mathlib): costs
            # wall/token budget, never a compile iteration.
            messages.append(_assistant_echo(message, with_tool_calls=True))
            _answer_info_calls(info_calls, unknown_calls)
            continue

        tool = _extract_tool_body(message) if config.tool_calls else None
        body: Optional[str]
        tool_call_id = ""
        if tool is not None:
            tool_call_id, body = tool
        else:
            body = _extract_fenced_body(message)
        if body is None or not body.strip():
            # No candidate this turn. The nudge that follows is a USER
            # message, so any tool_calls the reply carried are dropped
            # rather than echoed — an echoed tool_call_id that no tool
            # message answers is a 400 on the request after it. The
            # first such turn is free; the rest cost an iteration.
            if nudged:
                result.iterations += 1
            nudged = True
            messages.append(_assistant_echo(message, with_tool_calls=False))
            messages.append(
                {
                    "role": "user",
                    "content": "Call lean_run_code with your candidate body.",
                }
            )
            continue

        result.iterations += 1
        last_body = body
        # Structural pre-scan BEFORE burning a compile — candidate
        # BODIES only (info-tool outputs are tool turns, never scanned).
        banned = body_ban_scan(body)
        if banned is not None:
            feedback = (
                f"REJECTED before compiling: the token `{banned}` is banned in "
                "sidecar proof bodies (comments included). Submit a body "
                "without it."
            )
        else:
            check_started = now()
            verdict = compile_body(body)
            result.check_secs += max(0.0, now() - check_started)
            if verdict.ok:
                result.status = "success"
                result.proof_body = body
                break
            feedback = verdict.log[:4096]
        last_feedback = feedback

        messages.append(
            _assistant_echo(message, with_tool_calls=bool(tool_call_id))
        )
        if tool_call_id:
            # Answer every other tool call in the same assistant message
            # first (protocol: one result per tool_call_id) — info calls,
            # unknown tools, AND any further lean_run_code calls beyond
            # the one just compiled.
            extra_lean_calls = [
                cid
                for cid, name, _args in calls
                if name == "lean_run_code" and cid != tool_call_id
            ]
            _answer_info_calls(info_calls, unknown_calls, extra_lean_calls)
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": feedback,
                }
            )
        else:
            messages.append({"role": "user", "content": feedback})

    result.wall_secs = max(0.0, now() - started)
    result.transport_retries = getattr(client, "transport_retries", 0)
    if getattr(client, "effort_disabled", False):
        result.reasoning_effort = ""
    else:
        effort = (config.reasoning_effort or "").strip()
        result.reasoning_effort = "" if effort in ("", "none") else effort
    return result


def _assistant_echo(
    message: Dict[str, Any], *, with_tool_calls: bool
) -> Dict[str, Any]:
    """The reply as it goes back into the history.

    ``with_tool_calls`` is the caller's promise that it will answer
    every echoed id with a tool message (protocol: one result per
    tool_call_id, or the next request 400s); the paths that reply with
    a user message instead pass False and the calls are dropped."""
    # Thinking chunks are stripped here: never re-sent (the uncached free
    # tier re-bills the whole context every turn) — text only.
    echo: Dict[str, Any] = {
        "role": "assistant",
        "content": _content_text(message.get("content")),
    }
    if with_tool_calls and message.get("tool_calls"):
        echo["tool_calls"] = message["tool_calls"]
    if not echo["content"] and "tool_calls" not in echo:
        # An assistant message with neither text nor tool_calls is
        # REJECTED ("Assistant message must have either content or
        # tool_calls, but not none", HTTP 400) — and with reasoning
        # effort in force a turn spent entirely on thinking normalizes
        # to exactly that: the thinking is stripped, no text chunk is
        # left, and any tool call was dropped just above. The turn
        # happened and the history has to carry it, so the empty slot
        # states only what is true of it. Live: it ended two attempts on
        # the same node inside an hour, ~26 minutes of work apiece.
        echo["content"] = ASSISTANT_NO_TEXT_PLACEHOLDER
    return echo


# ---------------------------------------------------------------------------
# Spool record builder (§1.3 attempt schema)
# ---------------------------------------------------------------------------


def build_attempt_record(
    *,
    config: SidecarConfig,
    attempt_id: str,
    node: str,
    entry_seq: int,
    snapshot_sha: str,
    candidate: Dict[str, Any],
    workspace_fingerprints: Dict[str, str],
    result: AttemptResult,
    daemon_validation: Dict[str, Any],
    timings: Optional[Dict[str, float]] = None,
) -> Dict[str, Any]:
    provenance: Dict[str, Any] = {
        "provider": config.provider,
        "model": config.model_name,
        "iterations": result.iterations,
        "wall_secs": round(result.wall_secs, 3),
        "tokens": {
            "prompt": result.prompt_tokens,
            "completion": result.completion_tokens,
        },
        # v3 long-regime telemetry (additive; kernel serde is lenient).
        "compactions": result.compactions,
        "transport_retries": result.transport_retries,
        "reasoning_effort": result.reasoning_effort,
        "driver_version": DRIVER_VERSION,
    }
    # Phase telemetry (grunt-bench): additive, skipped when empty —
    # kernel-side parsing is serde-lenient and ignores it.
    merged_timings = attempt_timings(result, extra=timings)
    if merged_timings:
        provenance["timings"] = merged_timings
    return {
        "schema": 2,
        "attempt_id": attempt_id,
        "node": node,
        # Queue-entry generation (amendment A3): the kernel preflight
        # rejects a mismatch with the node's CURRENT queue entry as
        # `stale_generation`, closing the remove+re-add publish race.
        "entry_seq": int(entry_seq),
        "snapshot_sha": snapshot_sha,
        "base": {
            "node_file_sha256": str(candidate.get("node_file_sha256", "")),
            "statement_prefix_sha256": str(
                candidate.get("statement_prefix_sha256", "")
            ),
        },
        "workspace": dict(workspace_fingerprints),
        "artifact": {"proof_body": result.proof_body},
        "status": result.status,
        "provenance": provenance,
        "daemon_validation": dict(daemon_validation),
    }


def attempt_timings(
    result: AttemptResult, *, extra: Optional[Dict[str, float]] = None
) -> Dict[str, Any]:
    """Nonzero phase timings from a result, merged with attempt-level
    extras (refresh/server-open). Skip-when-default: zero phases are
    omitted so pre-telemetry consumers see no shape change."""
    timings: Dict[str, Any] = {}
    if result.api_secs:
        timings["api_secs"] = round(result.api_secs, 3)
    if result.check_secs:
        timings["check_secs"] = round(result.check_secs, 3)
    if result.info_tool_calls:
        timings["info_tool_calls"] = result.info_tool_calls
    if result.info_tool_counts:
        timings["info_tool_calls_by_kind"] = dict(
            sorted(result.info_tool_counts.items())
        )
    for key, value in (extra or {}).items():
        if value:
            timings[key] = round(float(value), 3)
    return timings
