"""Per-tablet Isabelle session SCAFFOLD generator (checker B2d).

The Isabelle analogue of the Lean tablet-support / umbrella generation
(``kernel/src/tablet_root.rs::generate_tablet_root_lean`` for the
``Tablet.lean`` umbrella; ``kernel/src/tablet_support.rs`` for the render).
It produces, under the per-tablet session directory ``<repo>/isabelle/``,
the three pieces the B2a checker builds + checks against:

* **``ROOT``** — ``session Tablet = HOL +`` with an ``options`` block and a
  ``theories`` list naming ``Tablet_Preamble`` first, then every present
  node theory ``Tablet_<Node>``. The session image the B2a server's
  ``use_theories`` + ``isabelle build -b`` materialize.
* **``Tablet_Preamble.thy``** — ``theory Tablet_Preamble imports Main begin
  … end``. The shared-imports base every node theory imports
  (``imports Tablet_Preamble``), the import-root analogue of
  ``Tablet/Preamble.lean``. GENERATED (not worker-authored): with no AFP
  installed the only base is ``Main``.
* **per-node ``Tablet_<Node>.thy``** — worker-AUTHORED, exactly like
  ``Tablet/<Node>.lean``. Isabelle's theory-name = file-stem coupling means
  the worker file already carries its own ``theory Tablet_<Node> imports
  Tablet_Preamble <cited Tablet_<Dep>…> begin <stmt+proof> end`` header
  (see ``kernel/src/isabelle_filespec.rs``). The generator therefore does
  NOT rewrite a node body; it owns only the session-level wiring (the ROOT
  listing + the generated Preamble base + a fail-closed stub for a node
  whose ``.thy`` is absent). This mirrors Lean precisely: the worker authors
  the full per-node file, the generator authors only the umbrella.

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

import re
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Sequence

# The fixed session name (the descriptor's umbrella is the ``ROOT`` file; the
# session it declares is ``Tablet``). Parent ``HOL`` memory-maps the prebuilt
# HOL heap warm (isabelle_install_notes.md §2/B.3).
SESSION_NAME = "Tablet"
SESSION_PARENT = "HOL"

# The generated shared-imports base theory (the ``Tablet/Preamble.lean``
# import-root analogue). Worker node theories ``imports Tablet_Preamble``.
PREAMBLE_THEORY = "Tablet_Preamble"
PREAMBLE_IMPORTS = "Main"

# Per-node theory-name mangling: file stem ``<Node>`` ⇒ theory ``Tablet_<Node>``
# (kernel/src/isabelle_filespec.rs; research 03 §D.1/D.5). ``<Node>`` already
# matches the node-name regex ``[A-Za-z][A-Za-z0-9_]*`` so the mangling yields
# a legal long-ident.
NODE_THEORY_PREFIX = "Tablet_"

# Node-name regex (mirrors trellis.checker.protocol.NODE_NAME_REGEX_STR / the
# Rust ``present_nodes`` discipline). Kept local so this module has no import
# cycle with the server.
_NODE_NAME_RE = re.compile(r"\A[A-Za-z][A-Za-z0-9_]*\Z")

# ROOT ``options`` block (the soundness-relevant build policy).
#   * ``quick_and_dirty = false`` — a stray ``sorry``/``oops`` FAILS the build
#     rather than being silently admitted (the Isabelle analogue of refusing
#     ``sorryAx``). This is the build-level soundness floor.
#   * ``threads`` / ``parallel_proofs`` — bounded so a build is courteous
#     alongside a co-resident run (isabelle_install_notes.md §2/A.2).
#   * ``timeout`` — a wall-clock ceiling so a runaway elaboration cannot wedge
#     the build (seconds; HOL is prebuilt so a per-tablet build is light).
# Deliberately NO ``proofs = 1`` / ``record_proofs``: neither is a valid option
# name in Isabelle2025-2 (``options [proofs=1]`` FAILS the build) and oracle/
# axiom NAMES record at level 0 regardless, which is all the sorry/skip_proof
# soundness check needs (isabelle_install_notes.md §7/A.5).
DEFAULT_ROOT_OPTIONS: "Dict[str, object]" = {
    "quick_and_dirty": False,
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


def generate_preamble_thy() -> str:
    """The generated ``Tablet_Preamble.thy`` body (the import-root base)."""
    return (
        f"theory {PREAMBLE_THEORY}\n"
        f"  imports {PREAMBLE_IMPORTS}\n"
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


def generate_root(
    node_theories: Sequence[str],
    *,
    options: "Optional[Dict[str, object]]" = None,
) -> str:
    """Render the session ``ROOT`` text.

    ``node_theories`` is the (already sorted) list of node theory names
    (``Tablet_<Node>``); ``Tablet_Preamble`` is listed first unconditionally
    so the import root is always built before the nodes that import it.
    """
    opts = DEFAULT_ROOT_OPTIONS if options is None else options
    lines: List[str] = []
    lines.append("(* Auto-generated by .trellis. Do not edit. *)")
    lines.append(f"session {SESSION_NAME} = {SESSION_PARENT} +")
    lines.append("  options [" + ", ".join(
        f"{key} = {_format_option(opts[key])}" for key in opts
    ) + "]")
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
) -> ScaffoldRender:
    """Pure render of the whole scaffold from a node-stem set.

    ``present_node_stems`` are the bare node ids ``<Node>`` whose worker
    ``Tablet_<Node>.thy`` is on disk; ``absent_node_stems`` are registered
    nodes whose ``.thy`` is missing (a fail-closed stub is emitted + they are
    listed in ROOT). Both are validated + sorted for byte-stability. The
    Preamble pseudo-node (``Preamble``) is never a node theory: it maps to the
    generated ``Tablet_Preamble`` base, so it is dropped from the node lists.
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
        preamble_text=generate_preamble_thy(),
        node_stub_texts=stub_texts,
    )


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
        if _NODE_NAME_RE.fullmatch(node) is not None:
            stems.append(node)
    return stems


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

    Returns a JSON-able summary (the written paths + the node theory list)
    the ``isabelle_sync_session`` handler echoes back.
    """
    session_dir.mkdir(parents=True, exist_ok=True)

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

    render = render_scaffold(present, options=options, absent_node_stems=absent)

    written: List[str] = []

    root_path = session_dir / "ROOT"
    root_path.write_text(render.root_text, encoding="utf-8")
    written.append(str(root_path))

    preamble_path = session_dir / f"{PREAMBLE_THEORY}.thy"
    preamble_path.write_text(render.preamble_text, encoding="utf-8")
    written.append(str(preamble_path))

    for theory, body in render.node_stub_texts.items():
        stub_path = session_dir / f"{theory}.thy"
        stub_path.write_text(body, encoding="utf-8")
        written.append(str(stub_path))

    return {
        "session_dir": str(session_dir),
        "root_path": str(root_path),
        "preamble_path": str(preamble_path),
        "node_theories": list(render.node_theories),
        "updated_paths": written,
    }


__all__ = [
    "SESSION_NAME",
    "SESSION_PARENT",
    "PREAMBLE_THEORY",
    "PREAMBLE_IMPORTS",
    "NODE_THEORY_PREFIX",
    "DEFAULT_ROOT_OPTIONS",
    "ScaffoldRender",
    "node_theory_name",
    "generate_preamble_thy",
    "generate_node_stub_thy",
    "generate_root",
    "render_scaffold",
    "sync_session",
]
