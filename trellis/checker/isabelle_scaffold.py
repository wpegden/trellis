"""Per-tablet Isabelle session SCAFFOLD generator (checker B2d).

The Isabelle analogue of the Lean tablet-support / umbrella generation
(``kernel/src/tablet_root.rs::generate_tablet_root_lean`` for the
``Tablet.lean`` umbrella; ``kernel/src/tablet_support.rs`` for the render).
It produces, under the per-tablet session directory ``<repo>/isabelle/``,
the three pieces the B2a checker builds + checks against:

* **``ROOT``** — a warm ``session Tablet_Base = HOL-Probability +`` base stanza
  followed by ``session Tablet = Tablet_Base +`` (each with an ``options``
  block); the ``Tablet`` stanza's ``theories`` list names ``Tablet_Preamble``
  first, then every present node theory ``Tablet_<Node>``. The session image the
  B2a server's ``use_theories`` + ``isabelle build -b`` materialize. The base
  session interposes the heavy distribution heap so warmth (analysis +
  probability + ``Complex_Main``) flows DOWN the parent edge into ``Tablet``.
* **``Tablet_Preamble.thy``** — ``theory Tablet_Preamble imports … begin
  end``. The shared-imports base every node theory imports
  (``imports Tablet_Preamble``), the import-root analogue of
  ``Tablet/Preamble.lean``. GENERATED (not worker-authored), but its
  ``imports`` are DERIVED from the worker's ``Tablet/Preamble.thy`` (Lean
  parity: the worker-authored ``Preamble.lean`` is honored by every build).
  Each derived import is validated against the Isabelle import allowlist and
  the whole derivation fails SAFE back to the historical ``Complex_Main``
  constant — see :func:`derive_preamble_imports`.
* **per-node ``Tablet_<Node>.thy``** — worker-AUTHORED, exactly like
  ``Tablet/<Node>.lean``. Isabelle's theory-name = file-stem coupling means
  the worker file already carries its own ``theory Tablet_<Node> imports
  Tablet_Preamble <cited Tablet_<Dep>…> begin <stmt+proof> end`` header
  (see ``kernel/src/isabelle_filespec.rs``). The generator therefore does
  NOT rewrite a node body; it owns only the session-level wiring (the ROOT
  listing + the generated Preamble base + a fail-closed stub for a node
  whose ``.thy`` is absent). This mirrors Lean precisely: the worker authors
  the full per-node file, the generator authors only the umbrella.

Worker -> session projection
----------------------------
The worker authors ``Tablet/<Node>.thy`` (header ``theory Tablet_<Node>``),
which :func:`trellis.checker.sync.sync_tablet_dir` mirrors to
``<supervisor_repo>/Tablet/<Node>.thy``. Isabelle resolves a theory by
file-STEM, so the session dir needs the same body under the file name
``Tablet_<Node>.thy``. Before listing the node theories, :func:`sync_session`
projects each ``<supervisor_repo>/Tablet/<Node>.thy`` (the sibling of the
session dir, since ``session_dir == <supervisor_repo>/isabelle``) to
``<session_dir>/Tablet_<Node>.thy`` by a pure COPY+RENAME -- the body and the
``theory Tablet_<Node>`` header are byte-UNTOUCHED, only the file stem is
mangled. A stale sweep then removes any projected ``Tablet_<Node>.thy`` whose
source ``Tablet/<Node>.thy`` is gone, mirroring ``sync_tablet_dir``'s removal
sweep. The source dir is derived from the socket-trusted session dir, never
request-supplied (the ``repo_path``-forbidden trust property), and the
generated ``Tablet_Preamble.thy`` is never a projection source.

Trust / invocation
------------------
``isabelle_sync_session`` is a SERVER-ONLY op (the ``sync-tablet-support``
analogue). The checker server derives the session dir from its socket
runtime root (``<supervisor_repo>/isabelle/``) and calls
:func:`sync_session` directly — the repo is never request-supplied (the
``repo_path``-forbidden trust property). Nothing on the Lean scaffold path
imports this module, so the Lean ``Tablet.lean`` / ``sync_tablet_support``
generation is byte-untouched.

Determinism
-----------
Node theories are listed in sorted order (``BTreeSet`` parity with the Lean
umbrella) so the generated ROOT is byte-stable for a given node set.
"""

from __future__ import annotations

import logging
import os
import re
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Sequence

_LOGGER = logging.getLogger(__name__)

# The fixed session name (the descriptor's umbrella is the ``ROOT`` file; the
# session it declares is ``Tablet``).
SESSION_NAME = "Tablet"

# Warm BASE session (Option B): rather than parenting ``Tablet`` directly on the
# bare ``HOL`` heap, we interpose a tiny prebuilt ``Tablet_Base`` session whose
# parent is the heavy ``HOL-Probability`` distribution heap. Warmth flows DOWN
# the parent edge: because ``Tablet_Base`` parents on ``HOL-Probability`` (which
# transitively pulls ``HOL-Analysis`` + ``Complex_Main``), the ``Tablet`` session
# — and therefore every worker/checker ``use_theories`` against it — memory-maps
# that whole analysis+probability surface warm, with NO in-burst library build.
# ``Tablet_Base`` itself adds no theories yet (the parent heap already provides
# the breadth); it exists purely to pin a heap image the working ``Tablet``
# session can parent on. The ``HOL-Probability`` distribution heap is the
# prerequisite (prebuilt separately/once); ``Tablet_Base`` is the small image
# rebuilt on top at setup.
BASE_SESSION_NAME = "Tablet_Base"
BASE_SESSION_PARENT = "HOL-Probability"

# The session DIRECTORY for the base stanza. A ROOT may not declare two sessions
# in the SAME directory (``isabelle build`` fails ``Duplicate use of directory``).
# Both ``Tablet_Base`` and ``Tablet`` otherwise default to the ROOT's own dir, so
# the base stanza is pinned to a dedicated SUBDIR via an ``in "<subdir>"`` clause.
# ``Tablet_Base`` carries NO theory files (it only pins a heap image), so the
# subdir is empty; ``Tablet`` KEEPS the ROOT dir because the projected node
# theories (``Tablet_<Node>.thy``) live there and ``use_theories`` resolves them
# from that ``master_dir``. The generator creates the (empty) subdir at write
# time — Isabelle requires a declared session directory to exist on disk.
BASE_SESSION_DIR = "base"

# The working session parents on the warm base, not on bare ``HOL``.
SESSION_PARENT = BASE_SESSION_NAME

# The generated shared-imports base theory (the ``Tablet/Preamble.lean``
# import-root analogue). Worker node theories ``imports Tablet_Preamble``.
PREAMBLE_THEORY = "Tablet_Preamble"
# The FALLBACK import set for the generated base. ``Complex_Main`` (binomials,
# transcendentals, series, MacLaurin) covers the elementary finite-sum
# probability/analysis the manuscripts need, reached with no heavy session
# build (its dependencies all sit under the prebuilt HOL heap). The steady
# state DERIVES the imports from the worker's ``Tablet/Preamble.thy`` instead
# (:func:`derive_preamble_imports`); this constant is the fail-safe the
# derivation falls back to (byte-identical to the historical generated file),
# never a fail-open into a broader environment.
PREAMBLE_IMPORTS = "Complex_Main"

# Import allowlist for the DERIVED preamble imports. LOCKSTEP with the kernel's
# ``kernel/src/backend.rs`` ``ISABELLE_HOL_ALLOWED_IMPORT_PREFIXES`` (the
# single Rust source of truth, consumed by ``isabelle_filespec::
# validate_imports`` at the worker gate); the Python checker has no view of
# that constant, so the list is REPLICATED here verbatim — change BOTH or the
# scaffold and the worker gate disagree about what a preamble may import.
# An import is allowed iff it IS a listed root or is a dotted descendant of
# one (``HOL-Probability.Probability``).
ALLOWED_PREAMBLE_IMPORT_PREFIXES: "tuple[str, ...]" = (
    "HOL",
    "Main",
    "Complex_Main",
    "HOL-Analysis",
    "HOL-Probability",
)

# A PLAIN session-qualified theory name (the only shape a derived preamble
# import may take, quoted or bare): ASCII-letter-led components of letters /
# digits / ``_`` / ``'``, joined by ``.`` (theory qualification) or ``-`` (the
# session-name hyphen). Mirrors ``isabelle_filespec.rs``'s
# ``is_plain_quoted_theory_name``.
_THEORY_NAME_RE = re.compile(r"\A[A-Za-z][A-Za-z0-9_']*(?:[.\-][A-Za-z][A-Za-z0-9_']*)*\Z")

# The worker ``Tablet/Preamble.thy`` header shape the derivation accepts:
# ``theory Tablet_Preamble`` then ``imports <clause>`` then ``begin``. STRICT
# by design — anything else (missing header, no imports clause, a ``keywords``
# clause) is unparseable and falls back.
_PREAMBLE_HEADER_RE = re.compile(
    r"\A\s*theory\s+Tablet_Preamble\s+imports\s+(?P<clause>.*?)\bbegin\b",
    re.S,
)

# One imports-clause token: a double-quoted name (NO escapes — a backslash or
# embedded quote makes the clause unparseable) or a bare word. The bare arm is
# deliberately LOOSE (it may match a malformed name like ``Foo..Bar``); the
# strict `_THEORY_NAME_RE` shape check downstream rejects those, so parsing
# stays simple and validation stays strict.
_PREAMBLE_IMPORT_TOKEN_RE = re.compile(
    r"\s*(?:\"(?P<quoted>[^\"\\]+)\"|(?P<bare>[A-Za-z][A-Za-z0-9_.']*))"
)

# Per-node theory-name mangling: file stem ``<Node>`` ⇒ theory ``Tablet_<Node>``
# (kernel/src/isabelle_filespec.rs; research 03 §D.1/D.5). ``<Node>`` already
# matches the node-name regex ``[A-Za-z][A-Za-z0-9_]*`` so the mangling yields
# a legal long-ident.
NODE_THEORY_PREFIX = "Tablet_"

# Node-name regex (mirrors trellis.checker.protocol.NODE_NAME_REGEX_STR / the
# Rust ``present_nodes`` discipline). Kept local so this module has no import
# cycle with the server.
_NODE_NAME_RE = re.compile(r"\A[A-Za-z][A-Za-z0-9_]*\Z")

# The CHECKER-OWNED cert-probe theory suffix (``Tablet_<Node>__Cert``,
# ``isabelle_session.cert_probe_theory_name``). A ``*__Cert`` stem is NEVER a
# worker node: it is checker scratch written next to the node it certifies and
# is supposed to be swept before ROOT enumeration. But ``<Node>__Cert`` matches
# ``_NODE_NAME_RE`` (the double underscore is legal), so a probe file that ever
# OUTLIVES its sweep (e.g. a sync that ran while the worker ``Tablet/`` dir was
# transiently absent, so the sweep had no source set) would be mis-enumerated as
# the node ``<Node>__Cert`` and pulled into the cold build's ROOT / the warm
# prefix. Hardening H3: reject the suffix explicitly at every node-enumeration
# point so a stray probe can never be elaborated as a node, independent of the
# sweep. Kept verbatim (not imported from ``isabelle_session``) so this module
# stays import-cycle-free.
CERT_PROBE_SUFFIX = "__Cert"

# The CHECKER-OWNED in-flight-alias infix (``Tablet_<Node>__In_<sha12>``,
# ``isabelle_session.fresh_inflight_theory``). H1: a held-open warm session
# elaborates the in-flight node under a fresh CONTENT-KEYED alias so a worker
# proof-replacing edit re-elaborates fresh (a ``purge_theories``+reload of the
# same name corrupts the held-open document on 2025-2). Like ``__Cert`` it is
# checker scratch with NO worker source: it must never be enumerated as a node
# or pulled into the cold build's ROOT / the warm prefix, and it is swept before
# ROOT enumeration. ``<Node>__In_<sha>`` matches ``_NODE_NAME_RE`` (the double
# underscore is legal), so reject it explicitly at every node-enumeration point,
# exactly as for the cert probe. Infix (not suffix) match catches the sha tail.
INFLIGHT_ALIAS_INFIX = "__In_"


def _is_checker_scratch_node(node: str) -> bool:
    """True iff ``node`` is a CHECKER-OWNED scratch stem (cert probe or in-flight
    alias), never a worker node — so it is swept + never enumerated (H3/H1)."""
    return CERT_PROBE_SUFFIX in node or INFLIGHT_ALIAS_INFIX in node

# ROOT ``options`` block (the soundness-relevant build policy).
#   * ``quick_and_dirty = false`` — a stray ``sorry``/``oops`` FAILS the build
#     rather than being silently admitted (the Isabelle analogue of refusing
#     ``sorryAx``). This is the build-level soundness floor.
#   * ``threads`` / ``parallel_proofs`` — bounded so a build is courteous
#     alongside a co-resident run.
#   * ``timeout`` — a wall-clock ceiling so a runaway elaboration cannot wedge
#     the build (seconds; HOL is prebuilt so a per-tablet build is light).
#   * ``record_proofs = 0`` — pinned EXPLICITLY. Level 0 retains the theorem
#     dependency boxes and oracle names, which is exactly what the checker's cut
#     traversal needs to attribute an oracle to the proof that INTRODUCED it
#     (`isabelle_session._cut_walk_ml`) — i.e. to tell "this node's own `sorry`"
#     apart from "a declared child's `sorry`". It does NOT record explicit proof
#     terms (level 2), which would be expensive and would not reuse the prebuilt
#     HOL heap. NOTE: ``proofs = 1`` is indeed not a valid option name, but
#     ``record_proofs`` IS one in Isabelle2025-2 (`etc/options`); an earlier
#     comment here claimed otherwise and was wrong.
DEFAULT_ROOT_OPTIONS: "Dict[str, object]" = {
    # `quick_and_dirty` is deliberately NOT pinned here. A session's own
    # `options [...]` OVERRIDE a command-line `-o`, so pinning it in ROOT made the
    # policy un-selectable per invocation — and pinning it FALSE made one open
    # node fail the whole-session materialization build, which contradicts the
    # phase contract (nodes are not all closed until the run ends). Isabelle's
    # global default is already `false` (`etc/options`), so omitting it keeps
    # STRICT the default everywhere; the materialization build opts into
    # `quick_and_dirty=true` explicitly, and the completion gate keeps the strict
    # default. Soundness is enforced by the per-node certificate, not by this
    # option: PIDE materializes an explicit `sorry` as the `skip_proof` oracle,
    # which the cut traversal attributes to the proof that introduced it.
    "record_proofs": 0,
    "threads": 2,
    "parallel_proofs": 1,
    "timeout": 1800,
}


@dataclass(frozen=True)
class ScaffoldRender:
    """The pure (no-I/O) render of a session scaffold.

    ``node_theories`` is the sorted list of present node theory names
    (``Tablet_<Node>``, excluding the Preamble). ``root_text`` /
    ``preamble_text`` are the exact file bodies; ``node_stub_texts`` carries a
    fail-closed stub body for each node whose worker ``.thy`` is absent (keyed
    by theory name) so the session still parses + the missing node FAILS
    rather than silently dropping from the build.
    """

    node_theories: Sequence[str]
    root_text: str
    preamble_text: str
    node_stub_texts: Dict[str, str]


def node_theory_name(node: str) -> str:
    """``<Node>`` → ``Tablet_<Node>`` (the session-qualified theory name)."""
    return f"{NODE_THEORY_PREFIX}{node}"


def _validate_node(node: str) -> str:
    if not isinstance(node, str) or _NODE_NAME_RE.fullmatch(node) is None:
        raise ValueError(
            f"invalid Isabelle node name {node!r} (must match [A-Za-z][A-Za-z0-9_]*)"
        )
    return node


def _format_option(value: object) -> str:
    """Render a ROOT option value the way Isabelle's option syntax expects.

    Booleans are lowercase bare words (``true``/``false``); ints are bare
    decimals. (Strings would be quoted, but the policy uses none.)
    """
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    return f'"{value}"'


def _parse_preamble_imports(text: str) -> "Optional[List[str]]":
    """Parse the ``imports`` clause of a worker ``Tablet/Preamble.thy``.

    Returns the import names in source order (quoted spellings unwrapped), or
    ``None`` when the file does not have the strict expected shape (``theory
    Tablet_Preamble`` / ``imports`` / names / ``begin``; no comments, no
    escapes, nothing fancy). ``None`` ⇒ the caller falls back — the parser is
    deliberately strict so anything surprising fails SAFE.
    """
    header = _PREAMBLE_HEADER_RE.match(text)
    if header is None:
        return None
    clause = header.group("clause")
    names: List[str] = []
    pos = 0
    while pos < len(clause):
        token = _PREAMBLE_IMPORT_TOKEN_RE.match(clause, pos)
        if token is None or token.end() == pos:
            if clause[pos:].strip() == "":
                # Trailing whitespace only — the clause is fully consumed.
                break
            # Junk the token grammar cannot read (a comment, an escape, a
            # cartouche, a stray symbol): the whole clause is unparseable.
            return None
        names.append(token.group("quoted") or token.group("bare"))
        pos = token.end()
    return names or None


def _preamble_import_violation(name: str) -> "Optional[str]":
    """The reason ``name`` may not be a derived preamble import, or ``None``.

    Three rules, in order: the name must be a PLAIN session-qualified theory
    name; it must not be ``Tablet``-shaped (the preamble IS the import root —
    an intra-tablet import there would be a cycle/smuggling hazard; substring
    match, deliberately over-broad, mirroring the kernel cone-key rule); and
    its root must be on the allowlist (`ALLOWED_PREAMBLE_IMPORT_PREFIXES`).
    """
    if _THEORY_NAME_RE.fullmatch(name) is None:
        return f"{name!r} is not a plain theory name"
    if "Tablet" in name:
        return (
            f"{name!r} is Tablet-shaped -- the preamble is the import root and "
            f"may not import tablet theories"
        )
    if not any(
        name == prefix or name.startswith(prefix + ".")
        for prefix in ALLOWED_PREAMBLE_IMPORT_PREFIXES
    ):
        return (
            f"{name!r} is not under an allowed import root "
            f"{list(ALLOWED_PREAMBLE_IMPORT_PREFIXES)}"
        )
    return None


def derive_preamble_imports(worker_preamble_path: Path) -> "Optional[List[str]]":
    """The validated import list for the generated session preamble, DERIVED
    from the worker's ``Tablet/Preamble.thy`` — or ``None`` ⇒ fall back to the
    historical ``PREAMBLE_IMPORTS`` constant.

    FAIL-SAFE, never fail-open: an absent / unreadable / unparseable worker
    preamble, or ANY import that is Tablet-shaped, malformed, or off the
    allowlist, abandons the WHOLE derivation (no partial adoption) and logs
    loudly. Duplicates are dropped (first occurrence wins); source order is
    preserved otherwise.
    """
    try:
        text = worker_preamble_path.read_text(encoding="utf-8")
    except FileNotFoundError:
        # D7 — ERROR, not WARNING. On an established repo the worker preamble
        # has existed since setup; its disappearance is anomaly-grade, and the
        # D1 stickiness check turns it into a hard sync failure anyway.
        _LOGGER.error(
            "isabelle_scaffold: worker preamble %s is absent; generated session "
            "preamble falls back to the historical constant (imports %s)",
            worker_preamble_path,
            PREAMBLE_IMPORTS,
        )
        return None
    except (OSError, UnicodeDecodeError) as exc:
        _LOGGER.error(
            "isabelle_scaffold: worker preamble %s is unreadable (%s); generated "
            "session preamble falls back to the historical constant (imports %s)",
            worker_preamble_path,
            exc,
            PREAMBLE_IMPORTS,
        )
        return None
    names = _parse_preamble_imports(text)
    if names is None:
        _LOGGER.error(
            "isabelle_scaffold: worker preamble %s has no parseable imports "
            "clause; generated session preamble falls back to the historical "
            "constant (imports %s)",
            worker_preamble_path,
            PREAMBLE_IMPORTS,
        )
        return None
    deduped: List[str] = []
    for name in names:
        violation = _preamble_import_violation(name)
        if violation is not None:
            _LOGGER.error(
                "isabelle_scaffold: worker preamble %s import rejected: %s; "
                "generated session preamble falls back to the historical "
                "constant (imports %s)",
                worker_preamble_path,
                violation,
                PREAMBLE_IMPORTS,
            )
            return None
        if name not in deduped:
            deduped.append(name)
    return deduped


def _render_import_name(name: str) -> str:
    """Emit ``name`` in the generated imports clause: bare when it is a legal
    long-ident, double-quoted otherwise (the hyphenated session-qualified
    spelling ``"HOL-Probability.Probability"`` — a hyphen is a symbolic char,
    so the bare spelling would not parse)."""
    return name if _LONG_IDENT_RE.fullmatch(name) else f'"{name}"'


# The header comment that marks a generated preamble as DERIVED (vs the
# historical constant fallback, which renders byte-identically without it).
# `sync_session` keys its D1 stickiness check off this marker: once the on-disk
# preamble carries it, a later derivation failure must not silently narrow the
# session's import root back to `Complex_Main`.
DERIVED_PREAMBLE_MARKER = "imports derived from Tablet/Preamble.thy"

# Operator escape hatch for D1 stickiness (deliberate regression to the
# historical constant): set to "1" to allow overwriting a derived preamble with
# the fallback render.
PREAMBLE_ALLOW_FALLBACK_ENV = "TRELLIS_ISABELLE_PREAMBLE_ALLOW_FALLBACK"


def generate_preamble_thy(imports: "Optional[Sequence[str]]" = None) -> str:
    """The generated ``Tablet_Preamble.thy`` body (the import-root base).

    ``imports=None`` (the fallback) renders the historical constant body
    BYTE-IDENTICALLY (no header comment — pre-derivation cert-cache keys over
    the scaffold bytes stay valid whenever the fallback is in effect).
    A derived import list renders with a header comment stating the source.
    """
    # D6: an EMPTY list would render `imports \nbegin` — malformed. Treat it as
    # the fallback rather than emitting a broken theory header.
    if not imports:
        return (
            f"theory {PREAMBLE_THEORY}\n"
            f"  imports {PREAMBLE_IMPORTS}\n"
            f"begin\n\n"
            f"end\n"
        )
    rendered = " ".join(_render_import_name(name) for name in imports)
    return (
        f"(* Auto-generated by .trellis; imports derived from "
        f"Tablet/Preamble.thy. Do not edit. *)\n"
        f"theory {PREAMBLE_THEORY}\n"
        f"  imports {rendered}\n"
        f"begin\n\n"
        f"end\n"
    )


def generate_node_stub_thy(node: str) -> str:
    """A fail-closed stub for a node whose worker ``.thy`` is absent.

    Lists the theory (so the ROOT reference resolves) but states an
    unprovable goal left ``sorry``-free, so the build FAILS rather than
    silently admitting a missing node. Mirrors the Lean convention of a
    fail-closed placeholder for an absent node file.
    """
    theory = node_theory_name(node)
    return (
        f"theory {theory}\n"
        f"  imports {PREAMBLE_THEORY}\n"
        f"begin\n\n"
        f"text \\<open>Auto-generated fail-closed stub: the worker "
        f"{theory}.thy is absent. The build FAILS here by design so a "
        f"missing node is never silently dropped from the session.\\<close>\n\n"
        f"lemma {node}: False\n"
        f"  oops\n\n"
        f"end\n"
    )


def _format_options_block(opts: "Dict[str, object]") -> str:
    """The shared ``options [...]`` line both session stanzas emit."""
    return "  options [" + ", ".join(
        f"{key} = {_format_option(opts[key])}" for key in opts
    ) + "]"


# A session/parent NAME is an unquoted long-ident in ROOT syntax. A hyphenated
# distribution session like ``HOL-Probability`` is NOT a long-ident (the ``-`` is
# a symbolic char), so it MUST be double-quoted in the ``= <parent>`` position or
# Isabelle rejects the ROOT (``keyword "+" expected … symbolic identifier "-"``).
# A bare-ident name (``Tablet_Base``, ``HOL``) is left unquoted. Quoting a bare
# ident would also parse, but we keep the canonical unquoted form for those so
# the generated ROOT stays minimal/byte-stable.
_LONG_IDENT_RE = re.compile(r"\A[A-Za-z][A-Za-z0-9_.]*\Z")


def _quote_session_name(name: str) -> str:
    """Quote a session name iff it is not a bare long-ident (e.g. hyphenated)."""
    return name if _LONG_IDENT_RE.fullmatch(name) else f'"{name}"'


def generate_root(
    node_theories: Sequence[str],
    *,
    options: "Optional[Dict[str, object]]" = None,
) -> str:
    """Render the session ``ROOT`` text.

    ``node_theories`` is the (already sorted) list of node theory names
    (``Tablet_<Node>``); ``Tablet_Preamble`` is listed first unconditionally
    so the import root is always built before the nodes that import it.

    Two stanzas are emitted into the one ROOT (so ``isabelle build -D`` sees
    both): the warm ``Tablet_Base = HOL-Probability`` base FIRST (the heap image
    the working session parents on — warmth flows down the parent edge), then
    ``Tablet = Tablet_Base`` carrying the Preamble + node theories. ``Tablet_Base``
    needs no extra ``theories``: the ``HOL-Probability`` parent already provides
    the analysis+probability+``Complex_Main`` breadth warm.
    """
    opts = DEFAULT_ROOT_OPTIONS if options is None else options
    options_line = _format_options_block(opts)
    base_parent = _quote_session_name(BASE_SESSION_PARENT)
    lines: List[str] = []
    lines.append("(* Auto-generated by .trellis. Do not edit. *)")
    # Warm base session: parents on the heavy HOL-Probability distribution heap
    # so the working Tablet session below inherits it warm (no in-burst build).
    # Pinned to a dedicated subdir (``in "base"``) so it does not collide with
    # ``Tablet`` (which keeps the ROOT dir for its projected node theories) — two
    # sessions may not share a directory. The hyphenated parent is quoted.
    lines.append(
        f"session {BASE_SESSION_NAME} in \"{BASE_SESSION_DIR}\" = {base_parent} +"
    )
    lines.append(options_line)
    lines.append("")
    # Working session: parents on the warm base, carries Preamble + node theories.
    lines.append(f"session {SESSION_NAME} = {SESSION_PARENT} +")
    lines.append(options_line)
    lines.append("  theories")
    lines.append(f"    {PREAMBLE_THEORY}")
    for theory in node_theories:
        lines.append(f"    {theory}")
    return "\n".join(lines) + "\n"


def render_scaffold(
    present_node_stems: Iterable[str],
    *,
    options: "Optional[Dict[str, object]]" = None,
    absent_node_stems: Iterable[str] = (),
    preamble_imports: "Optional[Sequence[str]]" = None,
) -> ScaffoldRender:
    """Pure render of the whole scaffold from a node-stem set.

    ``present_node_stems`` are the bare node ids ``<Node>`` whose worker
    ``Tablet_<Node>.thy`` is on disk; ``absent_node_stems`` are registered
    nodes whose ``.thy`` is missing (a fail-closed stub is emitted + they are
    listed in ROOT). Both are validated + sorted for byte-stability. The
    Preamble pseudo-node (``Preamble``) is never a node theory: it maps to the
    generated ``Tablet_Preamble`` base, so it is dropped from the node lists.
    ``preamble_imports`` is the ALREADY-VALIDATED derived import list for the
    generated base (from :func:`derive_preamble_imports`); ``None`` renders
    the historical fallback constant.
    """
    present = sorted(
        {_validate_node(n) for n in present_node_stems if n != "Preamble"}
    )
    absent = sorted(
        {_validate_node(n) for n in absent_node_stems if n != "Preamble"}
    )
    # A node can't be both present and absent; present wins.
    absent = [n for n in absent if n not in set(present)]

    node_theories = [node_theory_name(n) for n in present] + [
        node_theory_name(n) for n in absent
    ]
    node_theories = sorted(node_theories)

    stub_texts = {node_theory_name(n): generate_node_stub_thy(n) for n in absent}

    return ScaffoldRender(
        node_theories=node_theories,
        root_text=generate_root(node_theories, options=options),
        preamble_text=generate_preamble_thy(preamble_imports),
        node_stub_texts=stub_texts,
    )


def _unchanged_on_disk(dst: Path, data: bytes) -> bool:
    """True iff ``dst`` already holds exactly ``data`` (byte-for-byte).

    Used to make the scaffold writes CONTENT-CONDITIONAL: re-writing a file with
    identical bytes still bumps its mtime, and on a held-open WARM ``isabelle
    server`` document a touched source file churns PIDE's per-theory version
    tracking — which, after a cert probe has loaded/purged a dependent theory,
    corrupts the document and makes the next ``use_theories`` fail "Illegal
    theory header" (live-confirmed). Skipping no-op writes keeps the warm
    document stable across the per-cycle / per-advisory ``sync_session``.
    """
    try:
        return dst.read_bytes() == data
    except OSError:
        return False


def _atomic_write_bytes(dst: Path, data: bytes) -> None:
    """Write ``data`` to ``dst`` via temp+rename in ``dst.parent`` (atomic on
    the same filesystem). Mirrors ``sync._atomic_write``: a crash mid-write
    leaves either the prior file or the fully-written new one, never a partial
    body. ``tempfile.mkstemp`` opens with ``O_EXCL`` so a pre-existing symlink
    at the temp path causes a retry, never a write-through.

    CONTENT-CONDITIONAL: a no-op write (identical bytes already on disk) is
    skipped so the file's mtime is not bumped — keeping a held-open warm
    document stable (see :func:`_unchanged_on_disk`)."""
    if _unchanged_on_disk(dst, data):
        return
    dst.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=dst.name + ".tmp.", dir=str(dst.parent))
    try:
        try:
            os.write(fd, data)
        finally:
            os.close(fd)
        os.replace(tmp_name, dst)
    except Exception:
        try:
            os.unlink(tmp_name)
        except OSError:
            pass
        raise


def _project_worker_tablet_into_session(session_dir: Path) -> List[str]:
    """Project worker ``Tablet/<Node>.thy`` files into the session dir.

    The source dir is the session dir's SIBLING ``Tablet/`` (since
    ``session_dir == <supervisor_repo>/isabelle``), so it is derived purely
    from the socket-trusted session dir — never a request field. For each
    ``Tablet/<Node>.thy`` whose stem ``<Node>`` is a legal node id (skipping
    the worker-side ``Preamble.thy``, whose session base is the GENERATED
    ``Tablet_Preamble.thy``), copy the body byte-for-byte to
    ``<session_dir>/Tablet_<Node>.thy`` via temp+rename. The ``theory
    Tablet_<Node>`` header the worker already authored is untouched — only the
    file stem is mangled (``<Node>`` -> ``Tablet_<Node>``).

    Then sweep stale projections: any ``<session_dir>/Tablet_<Node>.thy`` whose
    source ``Tablet/<Node>.thy`` no longer exists is removed (the generated
    ``Tablet_Preamble.thy`` is never swept — it has no worker source). Mirrors
    ``sync_tablet_dir``'s removal sweep so a dropped worker node does not leave
    a ghost theory in the ROOT.

    Returns the sorted list of projected node ids (for logging/tests).
    """
    source_dir = session_dir.parent / "Tablet"
    if not source_dir.is_dir():
        # No worker Tablet/ source dir exists (e.g. a directly-seeded session
        # dir, as in the unit fixtures, or before the first sync_tablet_dir).
        # There is nothing to project and — crucially — nothing to sweep: a
        # theory already sitting in the session dir is NOT a stale projection,
        # so leave it untouched. The production session dir always has the
        # sibling Tablet/, so this short-circuit only spares the no-source case.
        return []

    projected: List[str] = []
    live_sources: set[str] = set()

    for src in sorted(source_dir.glob("*.thy")):
        node = src.stem  # worker file stem == <Node> (header carries Tablet_)
        if node == "Preamble":
            continue
        if _NODE_NAME_RE.fullmatch(node) is None:
            continue
        live_sources.add(node)
        try:
            data = src.read_bytes()
        except OSError:
            # An unreadable worker source is skipped (not fatal); the node then
            # falls into the absent/stub path downstream rather than wedging the
            # whole sync.
            continue
        dst = session_dir / f"{node_theory_name(node)}.thy"
        _atomic_write_bytes(dst, data)
        projected.append(node)

    # Stale sweep: drop any projected Tablet_<Node>.thy whose worker source is
    # gone. The generated Tablet_Preamble.thy is never a projection, so leave
    # it. Only runs when the source dir exists, so a directly-seeded session
    # theory (no source dir) is never mistaken for a stale projection.
    for path in session_dir.glob(f"{NODE_THEORY_PREFIX}*.thy"):
        stem = path.stem
        if stem == PREAMBLE_THEORY:
            continue
        node = stem[len(NODE_THEORY_PREFIX):]
        # H3/H1: a ``*__Cert*`` probe or ``*__In_*`` in-flight alias is checker
        # scratch with no worker source — sweep it unconditionally (never a
        # persistent file) so it cannot survive into ROOT enumeration or the warm
        # prefix. Infix match catches the per-call unique probe/alias names too.
        if _is_checker_scratch_node(node):
            try:
                path.unlink()
            except FileNotFoundError:
                pass
            continue
        if _NODE_NAME_RE.fullmatch(node) is None:
            continue
        if node not in live_sources:
            try:
                path.unlink()
            except FileNotFoundError:
                pass

    return sorted(projected)


def _node_stems_in_session_dir(session_dir: Path) -> List[str]:
    """The node ids whose worker ``Tablet_<Node>.thy`` is present in the dir.

    Walks ``<session_dir>/Tablet_*.thy``, strips the ``Tablet_`` prefix, and
    drops the generated ``Tablet_Preamble.thy``. Only files whose stripped
    stem is a legal node id are returned (a stray/garbage file is ignored, not
    fatal — the generator is robust to a dirty dir).
    """
    if not session_dir.is_dir():
        return []
    stems: List[str] = []
    for path in session_dir.glob(f"{NODE_THEORY_PREFIX}*.thy"):
        stem = path.stem  # e.g. "Tablet_Foo"
        if stem == PREAMBLE_THEORY:
            continue
        node = stem[len(NODE_THEORY_PREFIX):]
        # H3/H1: a ``*__Cert*`` probe or ``*__In_*`` in-flight alias stem is
        # checker scratch, never a worker node — reject it explicitly so one that
        # outlived its sweep can never be enumerated as a node (both match
        # ``_NODE_NAME_RE`` otherwise). Infix match also catches the per-call
        # unique ``Tablet_<Node>__Cert_<nonce>`` / ``Tablet_<Node>__In_<sha>``.
        if _is_checker_scratch_node(node):
            continue
        if _NODE_NAME_RE.fullmatch(node) is not None:
            stems.append(node)
    return stems



def refresh_worker_seed_preamble(repo_path: Path) -> "Optional[List[str]]":
    """Re-derive the WORKER-facing seed `<repo>/isabelle/Tablet_Preamble.thy`.

    Two copies of the generated preamble exist and they must agree:

    * `<supervisor_repo>/isabelle/` — the checker's session dir, refreshed on
      every `isabelle_sync_session`.
    * `<worker_repo>/isabelle/` — the SEED written once at setup (the prewarm's
      `Tablet_Base` render). It is what the worker's own scratch builds and
      `isa-query` parent on, and nothing refreshed it after setup.

    So an edit to `Tablet/Preamble.thy` reached the checker but never the
    worker: the worker compiled against a narrower import root than the gate
    used, which reads to it as "the scaffold ignores my preamble" — the live
    system_feedback that surfaced this.

    Rewrites ONLY the preamble (the seed's `ROOT` is deliberately node-less;
    projections belong to the session dir, not here). Returns the derived
    imports, or `None` when the fallback applies.
    """
    seed_dir = repo_path / "isabelle"
    if not seed_dir.is_dir():
        return None
    imports = derive_preamble_imports(repo_path / "Tablet" / "Preamble.thy")
    target = seed_dir / f"{PREAMBLE_THEORY}.thy"
    body = generate_preamble_thy(imports)
    if not _unchanged_on_disk(target, body.encode("utf-8")):
        target.write_text(body, encoding="utf-8")
    return imports

def sync_session(
    session_dir: Path,
    *,
    node_stems: "Optional[Iterable[str]]" = None,
    options: "Optional[Dict[str, object]]" = None,
) -> Dict[str, object]:
    """Render + WRITE the session scaffold into ``session_dir``.

    Writes ``ROOT`` + ``Tablet_Preamble.thy`` (always) and a fail-closed stub
    for any ``node_stems`` member that lacks a worker ``Tablet_<Node>.thy``.
    When ``node_stems`` is ``None`` the present worker ``Tablet_<Node>.thy``
    files in ``session_dir`` define the node set (the steady state — the
    workers authored their files there); passing an explicit ``node_stems``
    lets a caller (or test) register the expected nodes so an absent one gets
    a fail-closed stub instead of vanishing.

    The generated ``Tablet_Preamble.thy`` imports are DERIVED from the worker
    ``Tablet/Preamble.thy`` (the session dir's sibling, same socket-trusted
    derivation as the node projection), validated against the import
    allowlist, and fall back to the historical ``Complex_Main`` constant on
    any surprise (:func:`derive_preamble_imports` — fail-safe, loudly logged).

    Returns a JSON-able summary (the written paths + the node theory list +
    the effective preamble imports) the ``isabelle_sync_session`` handler
    echoes back.
    """
    session_dir.mkdir(parents=True, exist_ok=True)
    # The base stanza declares ``in "<BASE_SESSION_DIR>"``; Isabelle requires a
    # declared session directory to exist on disk (else ``No such directory``).
    # ``Tablet_Base`` carries no theory files, so the subdir stays empty.
    (session_dir / BASE_SESSION_DIR).mkdir(parents=True, exist_ok=True)

    # Project worker Tablet/<Node>.thy -> session_dir/Tablet_<Node>.thy (and
    # sweep stale projections) BEFORE enumerating present node theories, so the
    # glob below lists the real worker theories rather than the fail-closed
    # stubs. Source dir is the session dir's sibling Tablet/, derived from the
    # socket-trusted session dir — never request-supplied.
    _project_worker_tablet_into_session(session_dir)

    present_on_disk = set(_node_stems_in_session_dir(session_dir))
    if node_stems is None:
        present = present_on_disk
        absent: set = set()
    else:
        expected = {n for n in node_stems if n != "Preamble"}
        present = {n for n in expected if n in present_on_disk}
        absent = {n for n in expected if n not in present_on_disk}
        # Worker files present on disk but not in the expected set are still
        # built (they are registered nodes the caller's set simply omitted) —
        # union them in so a sync never drops an authored node.
        present |= present_on_disk

    # Derive the generated preamble's imports from the worker Tablet/Preamble
    # (the session dir's sibling — socket-trusted, never request-supplied,
    # exactly like the node projection source). ``None`` ⇒ historical fallback.
    derived_preamble_imports = derive_preamble_imports(
        session_dir.parent / "Tablet" / "Preamble.thy"
    )
    # D1 — the derived state is STICKY. Once the on-disk generated preamble is a
    # derived render, a derivation failure must not silently narrow the session's
    # import root back to `Complex_Main`: every node would re-elaborate in a
    # different theory context, mass-invalidating cert-cache keys, and a node
    # accepted inside that window pins its approved corr fingerprint from
    # narrow-env prints — which reads as a reopen storm once the derived render
    # returns. A torn read of the worker preamble (the mirror copies in place,
    # non-atomically) is enough to trigger it. Keep the existing bytes and fail
    # the sync loudly instead; the operator can force the regression with
    # `PREAMBLE_ALLOW_FALLBACK_ENV`.
    preamble_path = session_dir / f"{PREAMBLE_THEORY}.thy"
    if derived_preamble_imports is None:
        try:
            on_disk = preamble_path.read_text(encoding="utf-8")
        except OSError:
            on_disk = ""
        if DERIVED_PREAMBLE_MARKER in on_disk:
            if os.environ.get(PREAMBLE_ALLOW_FALLBACK_ENV, "").strip() == "1":
                _LOGGER.error(
                    "isabelle scaffold: preamble derivation FAILED but %s=1 — "
                    "overwriting the derived preamble at %s with the historical "
                    "`%s` fallback; every node re-elaborates in a narrower "
                    "context and every cert-cache key changes",
                    PREAMBLE_ALLOW_FALLBACK_ENV,
                    preamble_path,
                    PREAMBLE_IMPORTS,
                )
            else:
                raise RuntimeError(
                    "preamble derivation failed while the session preamble at "
                    f"{preamble_path} is a DERIVED render. Refusing to narrow the "
                    f"import root back to `{PREAMBLE_IMPORTS}` (it would "
                    "re-elaborate every node in a different theory context). "
                    "Fix Tablet/Preamble.thy, or set "
                    f"{PREAMBLE_ALLOW_FALLBACK_ENV}=1 to force the regression."
                )

    render = render_scaffold(
        present,
        options=options,
        absent_node_stems=absent,
        preamble_imports=derived_preamble_imports,
    )

    written: List[str] = []

    # CONTENT-CONDITIONAL writes (skip no-op rewrites so the warm document is not
    # churned — see ``_atomic_write_bytes`` / ``_unchanged_on_disk``). ``written``
    # still lists every authoritative path (whether or not it was re-touched), so
    # the summary is unchanged.
    def _write_if_changed(path: Path, text: str) -> None:
        data = text.encode("utf-8")
        if not _unchanged_on_disk(path, data):
            path.write_text(text, encoding="utf-8")
        written.append(str(path))

    _write_if_changed(session_dir / "ROOT", render.root_text)
    _write_if_changed(session_dir / f"{PREAMBLE_THEORY}.thy", render.preamble_text)
    for theory, body in render.node_stub_texts.items():
        _write_if_changed(session_dir / f"{theory}.thy", body)

    return {
        "session_dir": str(session_dir),
        "root_path": str(session_dir / "ROOT"),
        "preamble_path": str(session_dir / f"{PREAMBLE_THEORY}.thy"),
        "node_theories": list(render.node_theories),
        "updated_paths": written,
        # Observability: the effective generated-preamble imports and whether
        # they were derived from the worker Tablet/Preamble.thy (False = the
        # historical fallback constant is in effect). Additive fields; the
        # kernel does not parse the summary.
        "preamble_imports": (
            list(derived_preamble_imports)
            if derived_preamble_imports is not None
            else [PREAMBLE_IMPORTS]
        ),
        "preamble_derived": derived_preamble_imports is not None,
    }


__all__ = [
    "SESSION_NAME",
    "SESSION_PARENT",
    "BASE_SESSION_NAME",
    "BASE_SESSION_PARENT",
    "BASE_SESSION_DIR",
    "PREAMBLE_THEORY",
    "PREAMBLE_IMPORTS",
    "ALLOWED_PREAMBLE_IMPORT_PREFIXES",
    "derive_preamble_imports",
    "NODE_THEORY_PREFIX",
    "CERT_PROBE_SUFFIX",
    "DEFAULT_ROOT_OPTIONS",
    "ScaffoldRender",
    "node_theory_name",
    "generate_preamble_thy",
    "generate_node_stub_thy",
    "generate_root",
    "render_scaffold",
    "sync_session",
]
