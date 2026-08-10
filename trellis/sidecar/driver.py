"""Sidecar driver primitives (E§3) — structurally body-frozen.

The only-BODY guarantee is STRUCTURAL: the sidecar alone writes the
node file, and it only ever splices model output below the byte-frozen
prefix (everything through the ``-- BODY`` line). The model cannot
edit imports, the statement, other nodes, or create files, because no
code path writes anything else.

What lives here is the arm-independent half of an attempt: the FILESPEC
split/splice, the ban scan, prompt assembly, the ``AttemptResult`` /
``CompileVerdict`` shapes every arm reports in, and the spool record
builder. The generator itself is ``codex_driver.run_attempt_codex`` —
the hand-rolled OpenAI-compatible chat loop that used to sit here (its
``ModelClient``, tool schemas, context compaction and transport
classification) was retired with the HTTP arm.

Dependencies: stdlib only.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple

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

{lean_run_code_trailer}"""

LEAN_RUN_CODE_TRAILER = """\
Use the lean_run_code tool to compile candidate bodies. Iterate on compiler
feedback until it compiles cleanly. When the compiler reports success, stop.
"""

# The workspace's pinned mathlib checkout (lake package layout).
MATHLIB_PACKAGE_REL = ".lake/packages/mathlib"


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
        lean_run_code_trailer=LEAN_RUN_CODE_TRAILER,
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
# The `get_goals` half — tool-specific, and correctly dropped by a caller
# whose agent has no tool calls.
V3_GOALS_GUIDANCE = """\
Use the get_goals tool to inspect the proof goal state (the ⊢ goals) at a line
of your last-checked body — after a failed lean_run_code, call it at the line
where the proof goes wrong to see the exact remaining goals and hypotheses
instead of reconstructing them from the error text.
"""

# The persistence half — mechanism-independent, and the ONLY text in the
# whole prompt that tells the model not to stop. It used to be bundled with
# the paragraph above, so a caller that disabled `get_goals` silently lost
# "keep working" too: the arm whose only budget is a wall was the one told
# to stop early. Kept separate so that cannot recur.
V3_PERSISTENCE = """\
This is a LONG task: keep working. If the conversation is compacted (you will
see a compaction summary you wrote yourself), pick the work back up from your
summary and continue — do not restart from scratch or repeat approaches your
summary marks as failed.
"""

V3_GUIDANCE = V3_GOALS_GUIDANCE + "\n" + V3_PERSISTENCE


def build_system_prompt_v2(
    repo: Path,
    node: str,
    node_content: str,
    *,
    search_enabled: bool = True,
    goals_enabled: bool = True,
    mathlib_source_enabled: bool = True,
    tools_available: bool = True,
) -> str:
    """v1 prompt + the tool-workflow guidance matching the advertised
    info tools (loogle omitted when it is not configured, the mathlib
    grep omitted when the workspace has no mathlib checkout; the v3
    get_goals + compaction guidance appended when ``goals_enabled``).

    ``tools_available=False`` is for a caller whose agent has NO tool
    calls at all — the codex-CLI arm, which has a shell instead. The
    per-tool flags cannot express that: `V4_PROSE_STEP`,
    `V4_TABLET_SEARCH_STEP` and `V4_COMPILE_STEP` are unconditional, so
    turning every flag off still emits a numbered workflow built on
    `read_file`, `search_tablet` and `lean_run_code` — three mechanisms
    that agent does not have. Its caller then appends the shell
    equivalents, and the agent is handed two contradicting workflows.

    So "which mechanism does this agent have" is a first-class input
    here rather than something inferred from which tools happen to be
    enabled. The mathematical content is identical either way; only the
    workflow section differs.
    """
    steps = [V4_PROSE_STEP.format(node=node), V4_TABLET_SEARCH_STEP]
    if search_enabled:
        steps.append(V4_LOOGLE_STEP)
    if mathlib_source_enabled:
        steps.append(V4_MATHLIB_SOURCE_STEP)
    steps.append(V4_COMPILE_STEP)
    base = build_system_prompt(repo, node, node_content)
    if not tools_available:
        # Drop the tool-call instruction the v1 template ends with; the
        # caller states the real mechanism.
        base = base.replace(LEAN_RUN_CODE_TRAILER, "").rstrip() + "\n"
        prompt = base
    else:
        prompt = (
            base
            + "\nTOOLS AND WORKFLOW (follow this order):\n"
            + _numbered_steps(steps)
            + "\n"
        )
    if goals_enabled:
        prompt += "\n" + V3_GUIDANCE
    elif not tools_available:
        # V3_GUIDANCE bundles the `get_goals` paragraph with the ONLY
        # "keep working" text in the prompt. A caller without tools still
        # needs the persistence half.
        prompt += "\n" + V3_PERSISTENCE
    return prompt


# ---------------------------------------------------------------------------
# Attempt result types (the shape every arm reports in)
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
    # Set when a round failed at the COMPILE step (a well-disciplined body
    # that does not yet build, worth handing back with the compiler's
    # errors) or came back UNCHANGED with wall still on the clock (the
    # round did nothing; the next one retries with the last real error).
    # A collateral edit, a tampered prefix or a banned token is a
    # discipline failure and gets no second turn. Never serialized — the
    # spool record is built field-by-field in `build_attempt_record`.
    retryable: bool = False


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
