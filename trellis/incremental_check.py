"""Worker-facing incremental Lean checker (``incremental-check``).

An advisory inner-loop accelerator that drives a warm ``lake env lean
--server`` over LSP/stdio to give ``lake build``-shaped pass/fail+errors in
a fraction of the time. It is NEVER authoritative: green here is necessary
but not sufficient. The deterministic worker check (``check_node`` /
``check_tablet`` via the checker socket) remains the only sign-off gate.

Architecture (see incremental_check_plan.md, including the REQUIRED
CORRECTIONS FROM AUDIT which this implements) — a burst-lived reconnectable
BROKER:

- A long-lived **broker process** owns ONE ``lake env lean --server``
  subprocess (holds its stdin/stdout pipes) and listens on a unix-domain
  socket under ``.trellis/scratch/lean-server/`` (``broker.sock`` + a
  ``broker.pid`` pidfile). The warm server therefore SURVIVES across separate
  ``incremental-check`` invocations within a burst, so a later late-in-proof
  edit re-uses the warm elaboration snapshots instead of paying a cold
  Mathlib + node elaboration every call. This is the load-bearing fix over
  the previous spawn-per-call model, which gave ~zero benefit in the real
  worker edit->check->edit loop.

- Each ``incremental-check Tablet.NodeName`` invocation is a thin **client**:
  connect to the socket, send the request, the broker drives
  didOpen/didChange -> waits ``$/lean/fileProgress`` terminal for the target
  URI -> reads publishDiagnostics -> returns a ``lake build``-shaped result
  (exit code + error text). The broker keeps the server warm between calls.

- **Lazy start:** the first client in a burst starts the broker
  (double-fork / setsid so it detaches from the client) but the broker stays
  INSIDE the burst's PID namespace (it does not escape the bwrap), so it dies
  with the burst. A stale pidfile/socket is reaped before starting.
  Subsequent clients just connect.

- **Serialize** requests in the broker (one server, one document at a time).

- Invalidation (MAJOR-2 / ISSUE-3, targeted): each OPEN document records the
  transitive-import fingerprint it was elaborated against. Before each check
  the broker compares every open document's live fingerprint and reloads
  ONLY the documents whose own imports changed (``didClose`` + drop state),
  preserving the warm state of unrelated open nodes. If the TARGET (or one of
  its open imports) changed, that call falls back to ``lake build`` (which
  rebuilds dependency oleans) so it never reports a stale-prefix green. If the
  dependent set cannot be determined safely the broker degrades to a full
  server restart + fallback for that call. (Earlier this nuked the WHOLE
  server on any cross-node closure difference — fixed.)

- Reliability (ISSUE-1): the ``lean --server`` is launched with a large stack
  (env + unlimited ``RLIMIT_STACK``) and ``maxHeartbeats 0`` / ``maxRecDepth
  10000`` injected into the document buffer so a warm mid-edit re-elaboration of
  a normal node does not crash. Injected lines sit after the import block and
  diagnostic coordinates are mapped back to the real file. Elaboration is left
  ASYNC (the Lean default): an offline test on a giant single-theorem node
  proved ``Elab.async false`` + ``LEAN_NUM_THREADS=1`` WEDGES the cold open (the
  body-elaboration worker never spawns), so async stays on and threads stay at
  the Lean default.

- Giant-node skip: a deliberately-huge monolith cannot be warmed reliably (its
  async-ON cold open is ~15 min and a warm mid-edit can crash -> fallback
  anyway), so before any server round-trip the client detects a giant node (line
  count over a threshold, or a preamble ``maxHeartbeats`` at/above a high
  ceiling) and falls straight through to ``lake build``; the supervisor prewarm
  SKIPS it. The real fix for giants is splitting them.

- Prewarm (ISSUE-2): ``incremental-check --prewarm <node>`` (alias
  ``--start``) lazy-starts the broker and elaborates the node once so the cold
  cost is paid at burst BEGIN; it is fully failure-open (always exits 0, never
  runs ``lake build``).

- Diagnostic translation (MAJOR-3): the broker waits for
  ``$/lean/fileProgress`` to reach the terminal (empty ``processing``) state
  for the TARGET document URI before reading diagnostics; surfaces errors
  reported on a different document URI; and treats "no terminal progress within
  the wait window" as ambiguous -> client falls back to ``lake build``. Severity
  mirrors ``lake build``: any error-severity diagnostic -> nonzero exit, while a
  ``declaration uses 'sorry'`` warning is reported as INFO (the location is
  surfaced but it never fails the check) — a proof-formalization node
  legitimately carries a PERMITTED open ``sorry`` and ``lake build`` exits 0 on
  it; no-sorry-at-submit is enforced by the deterministic worker check, not here.

- Failure-open (everywhere): on ANY failure (broker/socket unavailable,
  server wedged/crashed, protocol error, timeout, version mismatch, start
  failure, import change, anything ambiguous) the CLIENT prints a notice and
  falls back to ``lake build Tablet.NodeName``, returning that result. Worst
  case is wasted worker time, never a wrong "pass".

- Preference order (hybrid with the supervisor-side active-node prewarm
  server): a ``check`` first tries the supervisor's warm active-node server if
  ``INCREMENTAL_CHECK_ACTIVE_PREWARM_SOCK`` is set and reachable (fast first
  call on the active node, cold cost paid at prewarm); that server returns
  ``fallback`` for any non-active node, so the client then uses this in-burst
  broker, and finally ``lake build``. Each step is failure-open and inert when
  not configured. See ``trellis.active_node_prewarm``.

- Dies with the burst (MINOR-6): the primary guarantee is PID-namespace
  teardown — ``codex_headless.run()`` SIGHUPs bwrap PID 1 under
  ``--unshare-pid --die-with-parent`` on every exit path, reaping the broker
  AND its ``lean --server``. The detached broker stays inside that namespace.
  A pidfile/socket reaper is the secondary backstop.

- Sandbox-only (MINOR-5): the wrapper/broker refuse to run outside the bwrap
  (guard on the ``TMPDIR=/trellis-tmp`` tmpfs marker) so it can never
  host-lake a live tablet. The server is ``lake env lean --server`` (bare, no
  ``-o``), so it writes no oleans (MINOR-4) and runs no manifest
  reconciliation — strictly safer than the worker's own ``lake build``.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import resource
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path
from queue import Empty, Queue
from typing import Any, Dict, List, Mapping, Optional, Sequence, Tuple


# Single source of truth for the node-name shape — mirrors
# trellis.checker.protocol.NODE_NAME_REGEX_STR. Duplicated as a bare literal
# (no package import) so the provisioned script runs standalone inside the
# worker sandbox without the trellis package on sys.path.
_NODE_NAME_REGEX = re.compile(r"\A[A-Za-z][A-Za-z0-9_]*\Z")
_NODE_NAME_MAX_LEN = 128

# Import-line scan for transitive Tablet imports — mirrors
# observations._TABLET_IMPORT_RE / materialization_order / _direct_tablet_imports.
_TABLET_IMPORT_RE = re.compile(
    r"^\s*import\s+Tablet\.([A-Za-z0-9_']+)\s*$", re.MULTILINE
)

# LSP DiagnosticSeverity values (LSP spec).
_SEVERITY_ERROR = 1
_SEVERITY_WARNING = 2

# The Lean elaborator emits this warning when a declaration still contains a
# `sorry`. `lake build` surfaces it as a WARNING and exits 0, so we mirror that:
# the `sorry` location is reported as INFO, never as a failure (a
# proof-formalization node legitimately carries a PERMITTED open `sorry`). On
# v4.30.0-rc1 the message is ``declaration uses `sorry` `` (backticks);
# older/other phrasings use single quotes or none, so accept any (or no)
# surrounding delimiter.
_SORRY_WARNING_RE = re.compile(r"declaration uses [`'\"]?sorry[`'\"]?", re.IGNORECASE)

# How long to wait for the warm server to drive a single document to terminal
# fileProgress. Generous: a cold open of a large Mathlib-heavy node can take
# minutes. On expiry we treat the result as ambiguous and fall back to lake.
_PROGRESS_TIMEOUT_SECS = float(os.environ.get("INCREMENTAL_CHECK_PROGRESS_TIMEOUT", "1800"))
# How long to wait for the `initialize` handshake before declaring the server
# unusable and falling back.
_INIT_TIMEOUT_SECS = 120.0
# How long a client waits to connect to (a possibly just-started) broker
# before giving up and falling back. The broker accept loop is up almost
# immediately after fork; the server's cold elaboration happens lazily on the
# first request, not at startup, so this only covers socket creation.
_CONNECT_TIMEOUT_SECS = 30.0
# How long a client waits for the broker to answer ONE request. The broker
# does the real work (server drive + fileProgress wait), so this caps the
# whole round-trip and must exceed the progress timeout plus slack.
_REQUEST_TIMEOUT_SECS = _PROGRESS_TIMEOUT_SECS + 120.0
# Inactivity window: if the server emits NOTHING (no fileProgress, no
# diagnostics) for this long after a didOpen/didChange, we conclude there is
# no terminal fileProgress coming — either the change was a no-op the server
# coalesced/ignored, or the server wedged after partial activity — and treat
# the result as ambiguous (client falls back to lake). It must comfortably
# exceed the largest gap BETWEEN progress notifications during a legitimate
# elaboration (observed sub-second-to-few-seconds on v4.30.0-rc1, even for
# multi-minute nodes, because parallel elaboration streams progress
# continuously) while still bounding any hang to well under the overall cap.
_QUIET_TIMEOUT_SECS = float(os.environ.get("INCREMENTAL_CHECK_QUIET_TIMEOUT", "90"))


# --------------------------------------------------------------------------
# Server-reliability config (ISSUE-1)
#
# Web research established these settings are required to stop `lean --server`
# crashing on the deliberately-huge single-theorem Tablet nodes during a warm
# mid-edit re-elaboration. They are applied two ways:
#
#   * process env + a stack ulimit on the server subprocess (covers the
#     elaboration task-thread stacks and the main thread), and
#   * `set_option` lines injected into the document buffer (the elaboration
#     options proper) — there is no lakefile in the didOpen/didChange path to
#     carry `leanOptions`, and the Lean LSP exposes no per-request option
#     channel for these, so they ride in the buffer text (see _inject_options).
#
# The documented `--timeout`/`--memory` CLI flags are deliberately NOT used
# (research found them broken).
# --------------------------------------------------------------------------

# Env applied to the `lean --server` subprocess. A large stack covers the
# elaboration task-thread stacks (LEAN_STACK_SIZE_KB). LEAN_NUM_THREADS is left
# at the Lean default: an offline test proved that pinning it to 1 (together
# with `Elab.async false`) WEDGES the cold open on a giant single-theorem node —
# the body-elaboration worker thread never spawns — so the body worker needs the
# default thread pool to come up.
_SERVER_ENV = {
    "LEAN_STACK_SIZE_KB": "2097152",
}

# Elaboration options injected into the document buffer. Order is irrelevant;
# each is a standalone `set_option` (no `in`) so it applies to the rest of the
# file from its position. They are inserted directly AFTER the file's import
# block (Lean requires imports first), so only diagnostics at or below the
# insertion line need their coordinates shifted back (see _inject_options).
#   * maxHeartbeats 0    — never kill the proof on Lean's deterministic-timeout,
#                          so an advisory pre-check never reports a failure the
#                          worker cannot act on. This is deliberately MORE
#                          permissive than `lake build`: the Tablet package's
#                          leanOptions set only `autoImplicit false`, so a node
#                          builds at Lean's stock 200000 unless its own preamble
#                          raises the ceiling. A caller that needs the real build
#                          budget passes `max_heartbeats` (see _injected_options).
#   * maxRecDepth 10000  — headroom for deep recursive elaboration.
# Elaboration is left ASYNC (the Lean default; NO `Elab.async false`): the
# offline giant-node test showed `Elab.async false` futex-parks the server on a
# giant cold open (header elaborates, the body worker never spawns), while
# async-ON completes (~15.5 min) with correct diagnostics. Giant nodes that
# would still be slow/crash-prone to warm are skipped before any server
# round-trip (see `_is_giant_node`), so the remaining warm path is normal nodes,
# which re-elaborate fine under async.
_INJECTED_OPTIONS: Tuple[str, ...] = (
    "set_option maxHeartbeats 0",
    "set_option maxRecDepth 10000",
)

_MAXHEARTBEATS_OPTION_PREFIX = "set_option maxHeartbeats "


def _injected_options(max_heartbeats: Optional[int] = None) -> Tuple[str, ...]:
    """The options to inject, with the heartbeat budget optionally pinned.

    ``None`` is the broker's own behaviour and the default for every caller
    that does not ask otherwise: unbounded, as above. An int pins that budget
    instead — ``0`` still means unbounded, which is Lean's own convention.

    The pin is a DEFAULT, not an override: these lines sit immediately after
    the import block, so a node whose own preamble raises `maxHeartbeats`
    keeps its declared value, exactly as it does under `lake build`.
    """
    if max_heartbeats is None:
        return _INJECTED_OPTIONS
    pinned = f"{_MAXHEARTBEATS_OPTION_PREFIX}{int(max_heartbeats)}"
    return tuple(
        pinned if opt.startswith(_MAXHEARTBEATS_OPTION_PREFIX) else opt
        for opt in _INJECTED_OPTIONS
    )


# --------------------------------------------------------------------------
# Giant-node skip heuristic
#
# A deliberately-huge single-theorem monolith cannot be warmed reliably: its
# async-ON cold open is ~15 min, and a warm mid-edit re-elaboration can crash
# (-> lake fallback anyway), while async-OFF wedges outright. Warming it wastes
# time, so the client falls straight through to `lake build` and the supervisor
# prewarm skips it. The real fix for giants is splitting them.
#
# "Giant" is two cheap on-disk signals (no server, no parse):
#   * the .lean exceeds a line-count threshold (default 3000), and/or
#   * a preamble `set_option maxHeartbeats N` sits at/above a high ceiling
#     (default 2_000_000) — the marker the worker uses for a deliberately-huge
#     proof.
# The threshold mirrors the prewarm config knob `giant_node_max_lines`; here in
# the standalone worker copy it is an env override (the supervisor exports it).
# --------------------------------------------------------------------------

_GIANT_NODE_MAX_LINES = int(
    os.environ.get("INCREMENTAL_CHECK_GIANT_MAX_LINES", "3000")
)
_GIANT_NODE_HEARTBEAT_CEILING = int(
    os.environ.get("INCREMENTAL_CHECK_GIANT_HEARTBEAT_CEILING", "2000000")
)
# A preamble `set_option maxHeartbeats <N>` (allow `_` digit separators).
_MAXHEARTBEATS_RE = re.compile(
    r"set_option\s+maxHeartbeats\s+([0-9_]+)", re.MULTILINE
)


def is_giant_node(
    lean_path: Path,
    *,
    max_lines: int = _GIANT_NODE_MAX_LINES,
    heartbeat_ceiling: int = _GIANT_NODE_HEARTBEAT_CEILING,
) -> Tuple[bool, str]:
    """Return (is_giant, reason). A giant node is skipped before any warm-server
    round-trip (client -> lake fallback; supervisor prewarm -> skip).

    Cheap on-disk signals only: a line-count over ``max_lines``, or a preamble
    ``maxHeartbeats`` at/above ``heartbeat_ceiling``. Unreadable -> not giant
    (let the normal path report the read error). A non-positive ``max_lines``
    disables the line-count signal (the heartbeat signal still applies)."""
    try:
        content = lean_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return False, ""
    n_lines = content.count("\n") + 1
    if max_lines > 0 and n_lines > max_lines:
        return True, f"{n_lines} lines exceeds giant threshold {max_lines}"
    for m in _MAXHEARTBEATS_RE.finditer(content):
        try:
            val = int(m.group(1).replace("_", ""))
        except ValueError:
            continue
        if val >= heartbeat_ceiling >= 1:
            return (
                True,
                f"preamble maxHeartbeats {val} at/above ceiling "
                f"{heartbeat_ceiling}",
            )
    return False, ""


def _import_block_end_line(text: str) -> int:
    """Return the 0-based line index at which to insert `set_option` lines:
    the first line AFTER the file's leading import block.

    Lean requires every `import` to precede any command, so the options must
    go after the last import (and after a leading run of blank/comment lines
    that may sit between imports). We scan the leading region and stop at the
    first line that is neither blank, a line comment, nor an `import`.
    """
    lines = text.split("\n")
    insert_at = 0
    for idx, raw in enumerate(lines):
        stripped = raw.strip()
        if stripped.startswith("import "):
            # Insert directly after the LAST import line. Blank/comment lines
            # between imports are tolerated (we keep scanning), but the options
            # land right after the final import rather than after any trailing
            # comments — keeping the offset reasoning tight.
            insert_at = idx + 1
            continue
        if stripped == "" or stripped.startswith("--"):
            # Blank or comment line: could sit between two imports, so keep
            # scanning without moving the insertion point.
            continue
        # First real command line — stop.
        break
    return insert_at


def _inject_options(
    text: str, *, max_heartbeats: Optional[int] = None
) -> Tuple[str, int, int]:
    """Inject the elaboration `set_option` lines after the import block.

    Returns (injected_text, insert_line, num_injected_lines). The caller sends
    ``injected_text`` to the server but reports diagnostics against the REAL
    file, so it must shift any diagnostic line >= ``insert_line`` back by
    ``num_injected_lines`` (see _unshift_diag_line). Diagnostics ABOVE the
    insertion point (e.g. on an import line) are unaffected.

    ``max_heartbeats`` pins the injected heartbeat budget (see
    ``_injected_options``); the line COUNT is unchanged either way, so the
    diagnostic geometry a caller has already recorded stays valid.
    """
    insert_at = _import_block_end_line(text)
    options = _injected_options(max_heartbeats)
    lines = text.split("\n")
    n = len(options)
    new_lines = lines[:insert_at] + list(options) + lines[insert_at:]
    return "\n".join(new_lines), insert_at, n


def _unshift_diag_line(line0: int, insert_line: int, num_injected: int) -> int:
    """Map a 0-based diagnostic line in the INJECTED buffer back to the real
    file. Lines at or below the injected block shift up by num_injected; the
    injected lines themselves (which should never carry a real diagnostic) map
    to the insertion point."""
    if line0 < insert_line:
        return line0
    if line0 < insert_line + num_injected:
        return insert_line
    return line0 - num_injected


# --------------------------------------------------------------------------
# Sandbox guard (MINOR-5)
# --------------------------------------------------------------------------

def _sandbox_markers() -> Tuple[bool, str]:
    """Return (inside_bwrap, detail).

    The worker bwrap sets ``TMPDIR=/trellis-tmp`` (a tmpfs mounted only inside
    the sandbox, see ``sandbox.py`` ``_SANDBOX_TMPDIR``) and runs under
    ``--unshare-pid`` so it is PID 1 in its own namespace. Either marker alone
    can appear by accident; together they are a reliable bwrap-only signal. We
    require the tmpfs marker (the load-bearing one — it proves the worker
    ELAN/PATH wrapping is in effect) and accept it as sufficient.
    """
    tmpdir = os.environ.get("TMPDIR", "")
    if tmpdir == "/trellis-tmp" and Path("/trellis-tmp").is_dir():
        return True, ""
    return (
        False,
        "incremental-check must run inside the worker sandbox "
        f"(expected TMPDIR=/trellis-tmp, got {tmpdir!r}); refusing to run "
        "outside the bwrap to avoid host-lake.",
    )


def _allow_unsandboxed() -> bool:
    """Escape hatch for the OFFLINE validation harness ONLY.

    Set ``INCREMENTAL_CHECK_ALLOW_UNSANDBOXED=1`` to bypass the bwrap guard.
    Never set this in a live worker burst — it exists so the Phase-0/Phase-1
    offline battery can exercise the wrapper against a scratch repo copy. It
    is deliberately a separate env var from any live-run variable so it cannot
    be tripped accidentally.
    """
    return os.environ.get("INCREMENTAL_CHECK_ALLOW_UNSANDBOXED", "") == "1"


# --------------------------------------------------------------------------
# Node -> path resolution
# --------------------------------------------------------------------------

def _validate_node_name(node_name: str) -> str:
    cleaned = node_name.strip()
    if not cleaned:
        raise ValueError("node name is empty")
    if len(cleaned) > _NODE_NAME_MAX_LEN:
        raise ValueError(f"node name exceeds {_NODE_NAME_MAX_LEN} chars")
    if _NODE_NAME_REGEX.fullmatch(cleaned) is None:
        raise ValueError(f"node name does not match identifier shape: {cleaned!r}")
    return cleaned


def _parse_target(arg: str) -> str:
    """Accept ``Tablet.NodeName`` or bare ``NodeName``; return ``NodeName``."""
    raw = arg.strip()
    if raw.startswith("Tablet."):
        raw = raw[len("Tablet."):]
    return _validate_node_name(raw)


def _tablet_lean_path(repo: Path, node_name: str) -> Path:
    return repo / "Tablet" / f"{node_name}.lean"


def _direct_tablet_imports_in_text(text: str) -> List[str]:
    """Direct ``import Tablet.X`` names scanned from arbitrary TEXT (an overlay
    buffer), not from disk. The on-disk variant reads the file and delegates
    here so the scan is identical for the accepted copy and the worker's
    in-flight edit."""
    return [m.group(1) for m in _TABLET_IMPORT_RE.finditer(text)]


def _direct_tablet_imports(repo: Path, node_name: str) -> List[str]:
    lean_path = _tablet_lean_path(repo, node_name)
    if not lean_path.exists():
        return []
    content = lean_path.read_text(encoding="utf-8", errors="replace")
    return _direct_tablet_imports_in_text(content)


def _missing_tablet_imports(
    repo: Path, node_name: str, overlay_text: str
) -> List[str]:
    """Transitive Tablet imports of ``node_name`` (with its body replaced by
    ``overlay_text``) that do NOT exist as ``Tablet/<dep>.lean`` under ``repo``.

    Used by the supervisor prewarm server to detect when its workspace mirror is
    STALE relative to the worker's edit: a worker that created a new helper this
    burst and imported it (``import Tablet.NewHelper``) names a dep the mirror
    will not have until acceptance/checkpoint. The closure is seeded from the
    OVERLAY's direct imports (so a freshly-added ``import`` is seen) and then
    follows the on-disk imports of each present dep; any dep whose ``.lean`` is
    absent is collected. The returned order is deterministic (closure order)."""
    missing: List[str] = []
    seen_missing: set[str] = set()
    visited: set[str] = set()

    def visit_dep(name: str) -> None:
        cleaned = name.strip()
        if not cleaned or cleaned in visited:
            return
        visited.add(cleaned)
        if not _tablet_lean_path(repo, cleaned).is_file():
            if cleaned not in seen_missing:
                seen_missing.add(cleaned)
                missing.append(cleaned)
            return  # cannot descend into a file we do not have
        for dep in _direct_tablet_imports(repo, cleaned):
            visit_dep(dep)

    for dep in _direct_tablet_imports_in_text(overlay_text):
        visit_dep(dep)
    return missing


def _transitive_import_closure(repo: Path, node_name: str) -> List[str]:
    """Transitive Tablet import closure of ``node_name`` (excluding itself),
    mirroring ``observations.materialization_order``."""
    order: List[str] = []
    visited: set[str] = set()

    def visit(name: str) -> None:
        cleaned = name.strip()
        if not cleaned or cleaned in visited:
            return
        visited.add(cleaned)
        for dep in _direct_tablet_imports(repo, cleaned):
            visit(dep)
        order.append(cleaned)

    visit(node_name)
    # Drop the node itself; we track its own buffer separately.
    return [n for n in order if n != node_name]


def _import_mtimes(repo: Path, node_name: str) -> Dict[str, float]:
    """mtime of every transitive Tablet import .lean (and its .olean, when
    present). A change in either is a stale-prefix risk (MAJOR-2)."""
    mtimes: Dict[str, float] = {}
    for dep in _transitive_import_closure(repo, node_name):
        lean = _tablet_lean_path(repo, dep)
        if lean.exists():
            mtimes[f"lean:{dep}"] = lean.stat().st_mtime
        olean = (
            repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / f"{dep}.olean"
        )
        if olean.exists():
            mtimes[f"olean:{dep}"] = olean.stat().st_mtime
    return mtimes


def _uri_for(repo: Path, node_name: str) -> str:
    return "file://" + str(_tablet_lean_path(repo, node_name).resolve())


def _node_name_for_uri(repo: Path, uri: str) -> Optional[str]:
    """Inverse of ``_uri_for`` for this repo's ``Tablet/`` directory: return
    the node name if ``uri`` names a ``Tablet/<Node>.lean`` under ``repo``,
    else None (a foreign URI we cannot reason about)."""
    if not uri.startswith("file://"):
        return None
    path = Path(uri[len("file://"):])
    try:
        tablet_dir = (repo / "Tablet").resolve()
        resolved = path.resolve() if path.is_absolute() else (repo / path).resolve()
    except OSError:
        return None
    if resolved.parent != tablet_dir or resolved.suffix != ".lean":
        return None
    name = resolved.stem
    try:
        return _validate_node_name(name)
    except ValueError:
        return None


def _range_content_change(old: str, new: str) -> Mapping[str, Any]:
    """Compute a single minimal-range LSP ``contentChange`` from ``old`` to
    ``new`` by stripping the common prefix and suffix.

    Sending a tight range (rather than a whole-document replacement) is what
    lets ``lean --server`` reuse its elaboration snapshots up to the edit
    point — a late-in-proof edit then re-elaborates only the changed suffix.
    The range is expressed in LSP (0-based line, UTF-16 character) terms; the
    Tablet sources are ASCII-dominant, so character == code unit here.
    """
    # Common prefix length.
    n = min(len(old), len(new))
    p = 0
    while p < n and old[p] == new[p]:
        p += 1
    # Common suffix length (not overlapping the prefix).
    s = 0
    while (
        s < (n - p)
        and old[len(old) - 1 - s] == new[len(new) - 1 - s]
    ):
        s += 1
    old_mid_end = len(old) - s  # end offset (exclusive) of the replaced region

    def line_col(text: str, offset: int) -> Tuple[int, int]:
        # 0-based line and character of byte/char offset within text.
        prefix = text[:offset]
        line = prefix.count("\n")
        last_nl = prefix.rfind("\n")
        col = offset - (last_nl + 1)
        return line, col

    start_line, start_col = line_col(old, p)
    end_line, end_col = line_col(old, old_mid_end)
    return {
        "range": {
            "start": {"line": start_line, "character": start_col},
            "end": {"line": end_line, "character": end_col},
        },
        "text": new[p:len(new) - s],
    }


# --------------------------------------------------------------------------
# LSP server harness
# --------------------------------------------------------------------------

class _LeanServer:
    """Minimal LSP client over ``lake env lean --server`` stdio.

    By default it spawns a BARE ``lake env lean --server`` (the in-burst worker
    broker, which already runs inside the worker bwrap and must NOT nest a second
    sandbox). The supervisor-side prewarm server, which runs on the host,
    instead passes a pre-built ``server_cmd`` + ``server_env`` that wrap the
    command under bwrap (``sandbox.wrap_command(role="lake_compiler")``) so the
    run repo's ``.lake/packages/<pkg>`` source checkouts are read-only and a
    ``lake env`` re-clone can never wipe ``.lake/packages`` (Gap-1 wipe-safety).
    The wrapping itself lives in the prewarm package (which may import the
    ``trellis`` source tree); THIS module must stay self-contained so it runs
    standalone inside the worker sandbox.
    """

    def __init__(
        self,
        repo: Path,
        *,
        server_cmd: Optional[Sequence[str]] = None,
        server_env: Optional[Mapping[str, str]] = None,
    ) -> None:
        self.repo = repo
        if server_env is not None:
            env = dict(server_env)
        else:
            env = dict(os.environ)
        # ISSUE-1 server-reliability env: large per-thread stack + single-thread
        # elaboration so a warm mid-edit re-elaboration of a huge node does not
        # crash. These cover the elaboration task-thread stacks that the buffer
        # `set_option` lines cannot.
        env.update(_SERVER_ENV)

        if server_cmd is not None:
            cmd: List[str] = list(server_cmd)
        else:
            cmd = ["lake", "env", "lean", "--server"]

        def _preexec() -> None:
            # Complementary to LEAN_STACK_SIZE_KB: raise the main-thread stack
            # rlimit to unlimited (best-effort; hard cap may forbid it).
            try:
                resource.setrlimit(
                    resource.RLIMIT_STACK,
                    (resource.RLIM_INFINITY, resource.RLIM_INFINITY),
                )
            except (ValueError, OSError):
                try:
                    soft, hard = resource.getrlimit(resource.RLIMIT_STACK)
                    resource.setrlimit(resource.RLIMIT_STACK, (hard, hard))
                except (ValueError, OSError):
                    pass

        # `lake env` sets LEAN_PATH from the repo's lake manifest so Mathlib +
        # Tablet oleans resolve. Running it bare (no `-o`) means the server
        # writes no oleans (MINOR-4: no .lake/build write race).
        self.proc = subprocess.Popen(
            cmd,
            cwd=str(repo),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            preexec_fn=_preexec,
        )
        self._id = 0
        self._q: "Queue[Optional[Mapping[str, Any]]]" = Queue()
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()
        self._errbuf: List[str] = []
        self._err = threading.Thread(target=self._read_err, daemon=True)
        self._err.start()

    @property
    def pid(self) -> int:
        return self.proc.pid

    def _read_err(self) -> None:
        assert self.proc.stderr is not None
        for line in self.proc.stderr:
            self._errbuf.append(line.decode("utf-8", errors="replace"))

    def _read_loop(self) -> None:
        f = self.proc.stdout
        assert f is not None
        while True:
            header = b""
            while b"\r\n\r\n" not in header:
                c = f.read(1)
                if not c:
                    self._q.put(None)
                    return
                header += c
            clen = 0
            for hl in header.decode("utf-8", errors="replace").split("\r\n"):
                if hl.lower().startswith("content-length:"):
                    try:
                        clen = int(hl.split(":", 1)[1].strip())
                    except ValueError:
                        clen = 0
            body = b""
            while len(body) < clen:
                chunk = f.read(clen - len(body))
                if not chunk:
                    self._q.put(None)
                    return
                body += chunk
            try:
                self._q.put(json.loads(body.decode("utf-8")))
            except Exception:
                # Skip an unparseable frame rather than wedging the loop.
                continue

    def _send(self, obj: Mapping[str, Any]) -> None:
        data = json.dumps(obj).encode("utf-8")
        assert self.proc.stdin is not None
        self.proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
        self.proc.stdin.flush()

    def request(self, method: str, params: Mapping[str, Any]) -> int:
        self._id += 1
        self._send({"jsonrpc": "2.0", "id": self._id, "method": method, "params": params})
        return self._id

    def notify(self, method: str, params: Mapping[str, Any]) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def drain(self, timeout: float) -> Optional[Mapping[str, Any]]:
        try:
            return self._q.get(timeout=timeout)
        except Empty:
            return None

    def alive(self) -> bool:
        return self.proc.poll() is None

    def stderr_tail(self) -> str:
        return "".join(self._errbuf[-20:])

    def initialize(self) -> bool:
        rid = self.request(
            "initialize",
            {
                "processId": os.getpid(),
                "rootUri": "file://" + str(self.repo),
                "capabilities": {},
            },
        )
        deadline = time.time() + _INIT_TIMEOUT_SECS
        while time.time() < deadline:
            msg = self.drain(timeout=max(0.1, deadline - time.time()))
            if msg is None:
                if not self.alive():
                    return False
                continue
            if msg.get("id") == rid:
                self.notify("initialized", {})
                return True
        return False

    def shutdown(self) -> None:
        try:
            if self.alive():
                self.notify("exit", {})
                time.sleep(0.2)
        except Exception:
            pass
        try:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        except Exception:
            pass


# --------------------------------------------------------------------------
# Diagnostic translation (MAJOR-3)
# --------------------------------------------------------------------------

def _wait_terminal_progress(
    srv: _LeanServer, target_uri: str
) -> Tuple[str, List[Mapping[str, Any]], Dict[str, List[Mapping[str, Any]]]]:
    """Drive the server until the TARGET document reaches terminal
    fileProgress (empty ``processing``), collecting publishDiagnostics for
    every URI seen along the way.

    Returns (status, target_diags, diags_by_uri) where status is one of:
      - "complete":  terminal progress observed; diagnostics are authoritative
      - "ambiguous": no terminal progress within the window (caller -> lake)
      - "crashed":   server exited before terminal progress (caller -> lake)
    """
    start = time.time()
    # Inactivity is measured against TARGET-relevant traffic only. The server
    # also streams progress/diagnostics for other URIs and housekeeping
    # notifications; counting those would keep the inactivity clock alive even
    # after the target itself went quiet, so the clock would never expire on a
    # target that stalled mid-rebuild (observed with a `sorry` edit). Scoping
    # the clock to the target makes a stalled target fall back within the
    # quiet window.
    last_target_msg = start
    saw_processing = False
    # A FAST incremental didChange can re-elaborate so quickly that the server
    # emits only EMPTY-processing fileProgress (never a non-empty one) for the
    # target, plus the fresh target diagnostics — so the `saw_processing` latch
    # alone would wrongly call it ambiguous. The authoritative "this version was
    # elaborated" pair is: an empty-processing fileProgress for the target AND a
    # target publishDiagnostics (even an empty array clearing prior errors).
    # Once BOTH have been observed (in either order) we complete. This cannot
    # revive the false-early-complete bug, whose signature is an initial
    # empty-processing with NO target diagnostics ever published.
    saw_empty_progress = False
    saw_target_diags = False
    diags_by_uri: Dict[str, List[Mapping[str, Any]]] = {}

    def record(params: Mapping[str, Any]) -> None:
        uri = params.get("uri", "")
        diags_by_uri[uri] = list(params.get("diagnostics", []) or [])

    def _complete() -> Tuple[str, List[Mapping[str, Any]], Dict[str, List[Mapping[str, Any]]]]:
        # Give the server a brief grace window to flush a final
        # publishDiagnostics that may trail the progress notification.
        grace_end = time.time() + 1.0
        while time.time() < grace_end:
            m2 = srv.drain(timeout=max(0.05, grace_end - time.time()))
            if m2 is None:
                if not srv.alive():
                    break
                continue
            if m2.get("method") == "textDocument/publishDiagnostics":
                record(m2.get("params", {}))
        return "complete", diags_by_uri.get(target_uri, []), diags_by_uri

    while True:
        now = time.time()
        # Overall cap: a single elaboration should never exceed this.
        if now - start >= _PROGRESS_TIMEOUT_SECS:
            return "ambiguous", diags_by_uri.get(target_uri, []), diags_by_uri
        # Target-inactivity cap: if NO target-relevant message arrives for a
        # full quiet window, there is no terminal fileProgress coming — a
        # coalesced/ignored no-op edit, or a target that stalled after partial
        # activity. -> ambiguous, the client falls back to lake rather than
        # block on the overall cap. (Lean streams target progress
        # sub-second-to-few-seconds apart during a real elaboration, well
        # inside this window, even for multi-minute nodes.)
        quiet_for = now - last_target_msg
        if quiet_for >= _QUIET_TIMEOUT_SECS:
            return "ambiguous", diags_by_uri.get(target_uri, []), diags_by_uri
        wait_cap = _QUIET_TIMEOUT_SECS - quiet_for
        msg = srv.drain(timeout=max(0.05, wait_cap))
        if msg is None:
            if not srv.alive():
                return "crashed", diags_by_uri.get(target_uri, []), diags_by_uri
            continue
        method = msg.get("method")
        if method == "textDocument/publishDiagnostics":
            params = msg.get("params", {})
            if params.get("uri") == target_uri:
                last_target_msg = time.time()
                saw_target_diags = True
            record(params)
            # Fast incremental path: an empty-processing fileProgress already
            # arrived and now the fresh target diagnostics close the pair.
            if saw_empty_progress and saw_target_diags:
                return _complete()
        elif method == "$/lean/fileProgress":
            params = msg.get("params", {})
            if params.get("textDocument", {}).get("uri") == target_uri:
                last_target_msg = time.time()
                processing = params.get("processing", []) or []
                if processing:
                    saw_processing = True
                else:
                    saw_empty_progress = True
                    if saw_processing or saw_target_diags:
                        return _complete()


def _format_diag(
    repo: Path,
    node_name: str,
    diag: Mapping[str, Any],
    *,
    uri: str,
    inject_geometry: Optional[Tuple[int, int]] = None,
) -> str:
    rng = diag.get("range", {})
    start = rng.get("start", {})
    line0 = int(start.get("line", 0))
    target = _uri_for(repo, node_name)
    # Only the TARGET buffer carries injected set_option lines; map its
    # coordinates back to the real file. Foreign URIs are the unmodified
    # on-disk dependency files, so their coordinates are already real.
    if uri == target and inject_geometry is not None:
        insert_line, num_injected = inject_geometry
        line0 = _unshift_diag_line(line0, insert_line, num_injected)
    line = line0 + 1  # LSP is 0-based; lake is 1-based
    col = int(start.get("character", 0)) + 1
    sev = diag.get("severity", _SEVERITY_ERROR)
    label = "error" if sev == _SEVERITY_ERROR else "warning"
    message = str(diag.get("message", "")).strip()
    if uri == target:
        loc = f"Tablet/{node_name}.lean:{line}:{col}"
    else:
        # Error reported on a different document (MAJOR-3c): keep the foreign
        # path so the worker sees where it actually is.
        foreign = uri[len("file://"):] if uri.startswith("file://") else uri
        loc = f"{foreign}:{line}:{col}"
    return f"{loc}: {label}: {message}"


def _classify_diagnostics(
    repo: Path,
    node_name: str,
    diags_by_uri: Mapping[str, List[Mapping[str, Any]]],
    *,
    inject_geometry: Optional[Tuple[int, int]] = None,
) -> Tuple[bool, List[str], List[str]]:
    """Return (failed, error_lines, sorry_lines).

    Severity mirrors ``lake build``: a node FAILS only on an error-severity
    diagnostic on ANY document. A ``declaration uses 'sorry'`` warning is NOT a
    failure — during proof formalization the active node legitimately carries a
    PERMITTED open ``sorry`` on its authorized open branch, and ``lake build``
    itself reports ``sorry`` as a warning and exits 0. The deterministic worker
    check — not this advisory tool — enforces no-sorry-at-submit. So ``sorry``
    locations are surfaced as INFO (in ``sorry_lines``) rather than counted as
    failure; a worker that elaborates cleanly with only an expected ``sorry``
    remaining gets exit 0, and one that has fully closed the node sees no
    ``sorry`` info at all.

    ``error_lines`` are the rendered error diagnostics (always surfaced so the
    worker can act); ``sorry_lines`` are the rendered ``sorry`` locations.
    Plain warnings (neither error nor ``sorry``) are dropped, matching
    ``lake build`` which exits 0 with warnings.

    ``inject_geometry`` is ``(insert_line, num_injected)`` from the option
    injection so target-document diagnostic lines map back to the real file.
    """
    failed = False
    error_lines: List[str] = []
    sorry_lines: List[str] = []
    target_uri = _uri_for(repo, node_name)
    for uri, diags in diags_by_uri.items():
        for diag in diags:
            sev = diag.get("severity", _SEVERITY_ERROR)
            message = str(diag.get("message", ""))
            is_sorry = (
                sev == _SEVERITY_WARNING
                and uri == target_uri
                and _SORRY_WARNING_RE.search(message) is not None
            )
            rendered = _format_diag(
                repo, node_name, diag, uri=uri,
                inject_geometry=inject_geometry,
            )
            if sev == _SEVERITY_ERROR:
                failed = True
                error_lines.append(rendered)
            elif is_sorry:
                sorry_lines.append(rendered)
    return failed, error_lines, sorry_lines


def _build_verdict(
    failed: bool, error_lines: List[str], sorry_lines: List[str]
) -> Dict[str, Any]:
    """Assemble the broker/prewarm verdict dict shared by all consumers.

    A real error-severity diagnostic -> ``fail`` with the rendered error lines.
    Otherwise ``ok``. Either way, any ``sorry`` locations ride along under
    ``sorry`` so the client can surface them as INFO ("elaborates, but `sorry`
    remains at ...") without affecting the pass/fail outcome.
    """
    verdict: Dict[str, Any] = (
        {"verdict": "fail", "lines": error_lines} if failed else {"verdict": "ok"}
    )
    if sorry_lines:
        verdict["sorry"] = sorry_lines
    return verdict


# --------------------------------------------------------------------------
# Goal state at a position (advisory)
# --------------------------------------------------------------------------

# A goal query runs against an ALREADY-elaborated buffer, so it is a bare
# round-trip. Bounded tightly all the same: it is answered while the broker
# lock is held, and a wedged server must not block the agent's own next check.
_GOAL_TIMEOUT_SECS = 20.0


def _shift_line_into_buffer(
    line0: int, insert_line: int, num_injected: int
) -> int:
    """Inverse of ``_unshift_diag_line``: map a 0-based REAL-file line to its
    line in the option-injected buffer the server actually holds."""
    return line0 + num_injected if line0 >= insert_line else line0


def _plain_goal_at(
    srv: "_LeanServer",
    uri: str,
    goal_pos: Tuple[int, int],
    inject_geometry: Tuple[int, int],
) -> str:
    """Rendered goal state at a real-file position of the open document.

    ``goal_pos`` is (1-based line, 0-based column). Tries ``$/lean/plainGoal``
    (tactic position) and falls back to ``$/lean/plainTermGoal`` (term
    position, e.g. ``def f : T := sorry``), because a `sorry` occurs in both
    and neither request answers for the other.

    Purely advisory: EVERY failure degrades to an empty string. Nothing about
    a goal query may change a verdict or raise into the broker's request path.
    """
    line1, column = goal_pos
    insert_line, num_injected = inject_geometry
    doc_line = _shift_line_into_buffer(
        max(0, int(line1) - 1), insert_line, num_injected
    )
    position = {"line": doc_line, "character": max(0, int(column))}
    for method in ("$/lean/plainGoal", "$/lean/plainTermGoal"):
        try:
            rendered = _plain_goal_request(srv, method, uri, position)
        except Exception:  # noqa: BLE001 — advisory; never fatal
            return ""
        if rendered:
            return rendered
    return ""


def _plain_goal_request(
    srv: "_LeanServer",
    method: str,
    uri: str,
    position: Mapping[str, int],
) -> str:
    rid = srv.request(
        method, {"textDocument": {"uri": uri}, "position": dict(position)}
    )
    deadline = time.time() + _GOAL_TIMEOUT_SECS
    while time.time() < deadline:
        msg = srv.drain(timeout=0.5)
        if msg is None:
            if not srv.alive():
                return ""
            continue
        if msg.get("id") != rid:
            continue  # stray notification / another request's reply
        if msg.get("error"):
            return ""
        result = msg.get("result")
        if not isinstance(result, Mapping):
            return ""
        rendered = result.get("rendered") or result.get("goal")
        if isinstance(rendered, str) and rendered.strip():
            return rendered.strip()
        goals = result.get("goals")
        if isinstance(goals, list) and goals:
            return "\n---\n".join(str(g) for g in goals)
        return ""
    return ""


# --------------------------------------------------------------------------
# Broker state-dir paths
# --------------------------------------------------------------------------

def _server_state_dir(repo: Path) -> Path:
    return repo / ".trellis" / "scratch" / "lean-server"


def _socket_path(repo: Path) -> Path:
    return _server_state_dir(repo) / "broker.sock"


def _broker_pidfile(repo: Path) -> Path:
    return _server_state_dir(repo) / "broker.pid"


def _broker_logfile(repo: Path) -> Path:
    return _server_state_dir(repo) / "broker.log"


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


# --------------------------------------------------------------------------
# Length-prefixed JSON framing over the unix socket
# --------------------------------------------------------------------------

def _send_frame(sock: socket.socket, obj: Mapping[str, Any]) -> None:
    data = json.dumps(obj).encode("utf-8")
    sock.sendall(struct.pack(">I", len(data)) + data)


def _recv_exactly(sock: socket.socket, n: int) -> Optional[bytes]:
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def _recv_frame(sock: socket.socket) -> Optional[Mapping[str, Any]]:
    head = _recv_exactly(sock, 4)
    if head is None:
        return None
    (length,) = struct.unpack(">I", head)
    if length == 0 or length > (64 * 1024 * 1024):
        return None
    body = _recv_exactly(sock, length)
    if body is None:
        return None
    try:
        return json.loads(body.decode("utf-8"))
    except Exception:
        return None


# --------------------------------------------------------------------------
# Broker process
# --------------------------------------------------------------------------

class _Broker:
    """Long-lived broker: owns one warm ``lean --server`` and serializes
    ``incremental-check`` requests over a unix-domain socket.

    The broker is the only holder of the server's stdio, so the server stays
    warm across separate client invocations. Requests are serialized under a
    lock (one server, one document at a time). The broker restarts the server
    on a detected transitive-import change and answers that request with a
    ``fallback`` verdict so the client runs ``lake build`` (rebuilding
    dependency oleans).
    """

    def __init__(self, repo: Path) -> None:
        self.repo = repo
        self._lock = threading.Lock()
        self._srv: Optional[_LeanServer] = None
        # Per-(uri) document version + last-sent text, plus the
        # import-fingerprint the current server was started against (for
        # MAJOR-2 stale-prefix detection).
        self._open_docs: Dict[str, int] = {}
        # _open_text holds the INJECTED buffer text last sent for each URI (so a
        # warm didChange diffs injected-vs-injected and the server's view is
        # consistent). The option-injection geometry (insert line + count) is
        # tracked alongside so diagnostics can be mapped back to real-file
        # coordinates.
        self._open_text: Dict[str, str] = {}
        self._open_inject: Dict[str, Tuple[int, int]] = {}
        # Cached terminal verdict per URI for the exact text last elaborated,
        # so a re-check of an UNCHANGED document returns instantly instead of
        # sending a no-op didChange (which the server ignores -> no terminal
        # fileProgress -> would otherwise stall).
        self._last_verdict: Dict[str, Mapping[str, Any]] = {}
        # ISSUE-3 targeted invalidation: the import fingerprint each OPEN URI was
        # last elaborated against. A per-URI map (not one server-global value)
        # so a different node's differing closure does not look like an "import
        # change" and nuke the whole server; only the URIs whose own imports
        # actually changed are reloaded.
        self._doc_import_fp: Dict[str, Dict[str, float]] = {}
        self._stop = threading.Event()

    # -- server lifecycle --------------------------------------------------

    def _ensure_server(self, node_name: str) -> Tuple[Optional[_LeanServer], Optional[str]]:
        """Return (server, fallback_reason). Ensure a live server exists,
        starting one if needed. Stale-import handling is now TARGETED and runs
        in ``_invalidate_changed_imports`` (ISSUE-3); this only owns liveness.
        """
        if self._srv is not None and not self._srv.alive():
            self._teardown_server()

        if self._srv is None:
            ok = self._start_server()
            if not ok or self._srv is None:
                detail = ""
                if self._srv is not None:
                    detail = self._srv.stderr_tail().strip()[:200]
                self._teardown_server()
                return None, "lean --server failed to initialize: " + detail

        return self._srv, None

    def _start_server(self) -> bool:
        srv = _LeanServer(self.repo)
        if not srv.initialize():
            self._srv = srv  # keep so caller can read stderr_tail
            return False
        self._srv = srv
        self._open_docs = {}
        self._open_text = {}
        self._open_inject = {}
        self._last_verdict = {}
        self._doc_import_fp = {}
        return True

    def _teardown_server(self) -> None:
        if self._srv is not None:
            try:
                self._srv.shutdown()
            except Exception:
                pass
        self._srv = None
        self._open_docs = {}
        self._open_text = {}
        self._open_inject = {}
        self._last_verdict = {}
        self._doc_import_fp = {}

    def _drop_doc_state(self, uri: str) -> None:
        """Forget all per-URI state so the NEXT check of that node re-opens
        cleanly (used when a document is closed/invalidated)."""
        self._open_docs.pop(uri, None)
        self._open_text.pop(uri, None)
        self._open_inject.pop(uri, None)
        self._last_verdict.pop(uri, None)
        self._doc_import_fp.pop(uri, None)

    def _invalidate_changed_imports(self, node_name: str) -> bool:
        """ISSUE-3 targeted invalidation.

        For the target node AND every currently-open document, compare the
        live transitive-import fingerprint against the one each was last
        elaborated against. Any open document whose own imports changed is
        ``didClose``d and its warm state dropped (so it re-opens fresh next
        time, never serving a stale prefix); UNRELATED open documents keep
        their warm state.

        Returns True iff the call must fall back to ``lake build`` for the
        TARGET — i.e. the target itself or one of its (open) imports changed,
        so dependency oleans must rebuild before a trustworthy verdict. A
        change confined to an unrelated open node does NOT force the target to
        fall back.

        Conservative: any error computing the dependent set raises, and the
        broker's outer handler degrades the whole call to a full teardown +
        fallback rather than risk a stale green.
        """
        srv = self._srv
        if srv is None:
            return False

        target_uri = _uri_for(self.repo, node_name)
        # The set of (open) URIs to re-check for import drift: every open doc,
        # plus the target even if not yet open.
        uris = set(self._open_text.keys())
        uris.add(target_uri)

        target_must_fallback = False
        for uri in list(uris):
            dep_node = _node_name_for_uri(self.repo, uri)
            if dep_node is None:
                # Foreign / unrecognized URI among open docs: cannot reason
                # about its imports safely -> close it to stay correct, and if
                # it is the target force a fallback.
                if uri in self._open_text:
                    self._did_close(uri)
                    self._drop_doc_state(uri)
                if uri == target_uri:
                    target_must_fallback = True
                continue
            live_fp = _import_mtimes(self.repo, dep_node)
            recorded_fp = self._doc_import_fp.get(uri)
            if recorded_fp is None:
                # Not yet elaborated (or freshly invalidated): nothing warm to
                # protect for this URI. If it is the target it will be opened
                # below; no fallback forced by absence alone.
                continue
            if recorded_fp != live_fp:
                # This open document's imports changed -> reload ONLY it.
                if uri in self._open_text:
                    self._did_close(uri)
                self._drop_doc_state(uri)
                if uri == target_uri:
                    target_must_fallback = True

        return target_must_fallback

    def _did_close(self, uri: str) -> None:
        if self._srv is None:
            return
        try:
            self._srv.notify(
                "textDocument/didClose",
                {"textDocument": {"uri": uri}},
            )
        except Exception:
            pass

    # -- request handling --------------------------------------------------

    def handle_request(self, req: Mapping[str, Any]) -> Mapping[str, Any]:
        """Process one client request. Returns a result dict:

          {"verdict": "ok"|"fail"|"fallback",
           "lines": [...],            # for fail
           "reason": "...",           # for fallback
          }

        Any internal exception degrades to a fallback verdict so the client
        always has a safe path (failure-open).

        Two optional request fields serve callers that need a verdict for
        text that is deliberately NOT on disk (they must never write the
        agent's working tree):

        * ``text`` — elaborate this buffer as a ``didChange`` OVERLAY on the
          node's warm document instead of the on-disk bytes. Same mechanism
          the supervisor prewarm server already uses for the worker's
          in-flight edit (``_active_prewarm_check``); no disk write anywhere.
        * ``goal`` — ``{"line": L, "column": C}`` (1-based line, 0-based
          column, REAL-file coordinates). The rendered goal state at that
          position rides back on the verdict under ``goal``. It costs one
          extra LSP round-trip, never an extra elaboration.
        """
        node_name = str(req.get("node", ""))
        try:
            node_name = _parse_target(node_name)
        except ValueError as exc:
            return {"verdict": "fallback", "reason": f"bad node name: {exc}"}

        lean_path = _tablet_lean_path(self.repo, node_name)
        if not lean_path.is_file():
            return {
                "verdict": "fallback",
                "reason": f"Tablet/{node_name}.lean does not exist",
            }

        overlay_text = req.get("text")
        overlay = str(overlay_text) if isinstance(overlay_text, str) else None
        goal_req = req.get("goal")
        goal_pos: Optional[Tuple[int, int]] = None
        if isinstance(goal_req, Mapping):
            try:
                goal_pos = (int(goal_req["line"]), int(goal_req.get("column", 0)))
            except (KeyError, TypeError, ValueError):
                goal_pos = None

        # Absent (the ordinary `incremental-check` request) means the broker's
        # own unbounded budget. Only a caller that asks gets a pinned one.
        max_heartbeats: Optional[int] = None
        if req.get("max_heartbeats") is not None:
            try:
                max_heartbeats = int(req["max_heartbeats"])
            except (TypeError, ValueError):
                max_heartbeats = None
            if max_heartbeats is not None and max_heartbeats < 0:
                max_heartbeats = None

        with self._lock:
            try:
                return self._handle_locked(
                    node_name,
                    lean_path,
                    overlay_text=overlay,
                    goal_pos=goal_pos,
                    max_heartbeats=max_heartbeats,
                )
            except Exception as exc:  # failure-open at the broker too
                self._teardown_server()
                return {
                    "verdict": "fallback",
                    "reason": f"broker error: {type(exc).__name__}: {exc}",
                }

    def _handle_locked(
        self,
        node_name: str,
        lean_path: Path,
        *,
        overlay_text: Optional[str] = None,
        goal_pos: Optional[Tuple[int, int]] = None,
        max_heartbeats: Optional[int] = None,
    ) -> Mapping[str, Any]:
        srv, fallback_reason = self._ensure_server(node_name)
        if fallback_reason is not None:
            return {"verdict": "fallback", "reason": fallback_reason}
        assert srv is not None

        uri = _uri_for(self.repo, node_name)

        # ISSUE-3: targeted stale-import invalidation. Reload only the open
        # documents whose own imports changed; unrelated open nodes keep their
        # warm state. If the TARGET (or one of its open imports) changed, fall
        # back to `lake build` for THIS call so dependency oleans rebuild —
        # never a stale-prefix green.
        target_must_fallback = self._invalidate_changed_imports(node_name)
        if target_must_fallback:
            return {
                "verdict": "fallback",
                "reason": "transitive import changed (stale prefix)",
            }

        if overlay_text is not None:
            raw_text = overlay_text
        else:
            raw_text = lean_path.read_text(encoding="utf-8")
        # Inject the elaboration set_options after the import block; remember the
        # geometry so diagnostics map back to real-file coordinates. A pinned
        # heartbeat budget changes the injected TEXT, which is what the verdict
        # cache below is keyed on — so a bounded request can never be served a
        # verdict earned under the unbounded default, or vice versa.
        text, insert_line, num_injected = _inject_options(
            raw_text, max_heartbeats=max_heartbeats
        )

        # Snapshot the import fingerprint BEFORE elaborating. Recording it after
        # would race a dep change that lands mid-elaboration (the server used
        # the old olean, but the post-change mtime would make the next check
        # think it was current) — a stale-green risk. Captured here, a dep that
        # changes during this elaboration is caught on the NEXT check.
        import_fp_at_send = _import_mtimes(self.repo, node_name)

        prev_inject = self._open_inject.get(uri)
        if (
            uri in self._open_docs
            and uri in self._open_text
            and prev_inject == (insert_line, num_injected)
        ):
            old_text = self._open_text[uri]
            if old_text == text and uri in self._last_verdict:
                # Unchanged document: a no-op didChange yields no terminal
                # fileProgress (the server ignores it), so re-serve the cached
                # terminal verdict for the exact text last elaborated.
                #
                # A goal request still has to be answered, and this is the
                # cheapest place to answer one: the buffer is already
                # elaborated, so the position query is a bare LSP round-trip
                # with no re-elaboration behind it.
                if goal_pos is None:
                    return self._last_verdict[uri]
                cached = dict(self._last_verdict[uri])
                cached["goal"] = _plain_goal_at(
                    srv, uri, goal_pos, (insert_line, num_injected)
                )
                return cached
            # Warm reuse: send a didChange carrying a MINIMAL range (common
            # prefix/suffix stripped) so Lean reuses its elaboration snapshots
            # up to the edit point — a late-in-proof edit then re-elaborates
            # only the changed suffix, the cross-call speedup we are after.
            change = _range_content_change(old_text, text)
            self._open_docs[uri] += 1
            version = self._open_docs[uri]
            srv.notify(
                "textDocument/didChange",
                {
                    "textDocument": {"uri": uri, "version": version},
                    "contentChanges": [change],
                },
            )
            self._open_text[uri] = text
        else:
            # Fresh open (or the injection geometry shifted, e.g. the import
            # block changed): close any prior open and re-open clean.
            if uri in self._open_docs:
                self._did_close(uri)
            self._open_docs[uri] = 1
            self._open_text[uri] = text
            srv.notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": uri,
                        "languageId": "lean",
                        "version": 1,
                        "text": text,
                    }
                },
            )
        self._open_inject[uri] = (insert_line, num_injected)

        status, _target_diags, diags_by_uri = _wait_terminal_progress(srv, uri)
        if status == "crashed":
            self._teardown_server()
            return {
                "verdict": "fallback",
                "reason": "lean --server crashed during elaboration",
            }
        if status == "ambiguous":
            # No usable terminal signal. Drop the cached/open state for this
            # URI so the NEXT call re-opens cleanly rather than diffing against
            # a now-uncertain server state.
            self._drop_doc_state(uri)
            self._teardown_server()
            return {
                "verdict": "fallback",
                "reason": "no terminal fileProgress within wait window",
            }

        failed, error_lines, sorry_lines = _classify_diagnostics(
            self.repo, node_name, diags_by_uri,
            inject_geometry=(insert_line, num_injected),
        )
        verdict: Dict[str, Any] = _build_verdict(failed, error_lines, sorry_lines)
        self._last_verdict[uri] = verdict
        # Record the import fingerprint THIS doc was elaborated against (the
        # pre-send snapshot), so a later check can detect (only) this node's
        # import drift without racing a mid-elaboration dep change.
        self._doc_import_fp[uri] = import_fp_at_send
        if goal_pos is not None:
            # Cached under a COPY so the goal string never enters the verdict
            # cache: the cache is keyed by buffer text alone, and a later
            # caller asking for a different position must not be served this
            # one's goal.
            with_goal = dict(verdict)
            with_goal["goal"] = _plain_goal_at(
                srv, uri, goal_pos, (insert_line, num_injected)
            )
            return with_goal
        return verdict

    # -- socket serve loop -------------------------------------------------

    def serve(self, sock_path: Path) -> None:
        srv_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            srv_sock.bind(str(sock_path))
        except OSError:
            # Lost a start race or stale node; bail (a peer broker owns it).
            return
        srv_sock.listen(8)
        srv_sock.settimeout(1.0)
        try:
            while not self._stop.is_set():
                try:
                    conn, _ = srv_sock.accept()
                except socket.timeout:
                    continue
                except OSError:
                    break
                threading.Thread(
                    target=self._serve_conn, args=(conn,), daemon=True
                ).start()
        finally:
            try:
                srv_sock.close()
            except OSError:
                pass
            self._teardown_server()
            try:
                sock_path.unlink()
            except OSError:
                pass

    def _serve_conn(self, conn: socket.socket) -> None:
        try:
            conn.settimeout(_REQUEST_TIMEOUT_SECS)
            req = _recv_frame(conn)
            if req is None:
                return
            method = req.get("method")
            if method == "ping":
                _send_frame(conn, {"verdict": "pong"})
                return
            if method == "shutdown":
                _send_frame(conn, {"verdict": "ok"})
                self._stop.set()
                return
            if method != "check":
                _send_frame(conn, {"verdict": "fallback", "reason": "unknown method"})
                return
            result = self.handle_request(req)
            _send_frame(conn, result)
        except Exception:
            try:
                _send_frame(conn, {"verdict": "fallback", "reason": "broker conn error"})
            except Exception:
                pass
        finally:
            try:
                conn.close()
            except OSError:
                pass


def _broker_main(repo: Path) -> int:
    """Entry point for the detached broker process."""
    state_dir = _server_state_dir(repo)
    state_dir.mkdir(parents=True, exist_ok=True)
    sock_path = _socket_path(repo)
    pidfile = _broker_pidfile(repo)

    # Reattach stdio to the broker log so a detached broker never blocks on a
    # closed client pipe and leaves a forensic trail.
    try:
        log = open(_broker_logfile(repo), "a", buffering=1)
        os.dup2(log.fileno(), 1)
        os.dup2(log.fileno(), 2)
    except OSError:
        pass

    pidfile.write_text(str(os.getpid()), encoding="utf-8")

    broker = _Broker(repo)

    def _on_signal(_sig: int, _frm: Any) -> None:
        broker._stop.set()

    for s in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        try:
            signal.signal(s, _on_signal)
        except (ValueError, OSError):
            pass

    try:
        broker.serve(sock_path)
    finally:
        broker._teardown_server()
        try:
            if pidfile.is_file():
                pidfile.unlink()
        except OSError:
            pass
    return 0


# --------------------------------------------------------------------------
# Broker lazy-start (double-fork / setsid; stays inside the PID namespace)
# --------------------------------------------------------------------------

def _reap_stale_broker(repo: Path) -> None:
    """Kill a recorded broker (if its pidfile is live) and remove a stale
    socket node. Used before a fresh start and on ``--shutdown``."""
    pidfile = _broker_pidfile(repo)
    if pidfile.is_file():
        try:
            pid = int(pidfile.read_text(encoding="utf-8").strip())
        except (ValueError, OSError):
            pid = -1
        if pid > 0 and _pid_alive(pid):
            try:
                os.kill(pid, signal.SIGTERM)
                deadline = time.time() + 3.0
                while time.time() < deadline and _pid_alive(pid):
                    time.sleep(0.1)
                if _pid_alive(pid):
                    os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
        try:
            pidfile.unlink()
        except OSError:
            pass
    sock_path = _socket_path(repo)
    try:
        if sock_path.exists() or sock_path.is_symlink():
            sock_path.unlink()
    except OSError:
        pass


def _spawn_broker(repo: Path) -> None:
    """Double-fork a detached broker that owns the warm server.

    The broker calls ``setsid`` so it is not in the client's foreground
    process group (it must outlive the client). It does NOT escape the PID
    namespace — it remains a child of bwrap PID 1, so the burst's
    ``--unshare-pid --die-with-parent`` teardown reaps it (MINOR-6). We
    double-fork so the broker is reparented to PID 1 (inside the namespace)
    rather than left as a zombie-producing child of the short-lived client.
    """
    # First fork.
    pid = os.fork()
    if pid > 0:
        # Parent (client): reap the intermediate child immediately.
        os.waitpid(pid, 0)
        return
    # Intermediate child.
    try:
        os.setsid()
    except OSError:
        pass
    pid2 = os.fork()
    if pid2 > 0:
        os._exit(0)
    # Grandchild: the actual broker. Detach stdio from the client.
    try:
        devnull = os.open(os.devnull, os.O_RDONLY)
        os.dup2(devnull, 0)
        os.close(devnull)
    except OSError:
        pass
    try:
        os._exit(_broker_main(repo))
    except Exception:
        os._exit(1)


def _wait_for_socket(repo: Path, deadline: float) -> bool:
    sock_path = _socket_path(repo)
    while time.time() < deadline:
        if sock_path.exists():
            # Confirm it is accepting connections (a ping round-trip).
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                    s.settimeout(2.0)
                    s.connect(str(sock_path))
                    _send_frame(s, {"method": "ping"})
                    resp = _recv_frame(s)
                    if resp is not None and resp.get("verdict") == "pong":
                        return True
            except OSError:
                pass
        time.sleep(0.1)
    return False


def _ensure_broker(repo: Path) -> bool:
    """Ensure a live, reachable broker. Returns True if reachable.

    Fast path: an existing live pidfile + responsive socket -> just use it.
    Otherwise reap stale state and lazy-start one.
    """
    sock_path = _socket_path(repo)
    pidfile = _broker_pidfile(repo)

    # Fast path: pidfile names a live process and the socket answers ping.
    if pidfile.is_file() and sock_path.exists():
        try:
            pid = int(pidfile.read_text(encoding="utf-8").strip())
        except (ValueError, OSError):
            pid = -1
        if pid > 0 and _pid_alive(pid):
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                    s.settimeout(2.0)
                    s.connect(str(sock_path))
                    _send_frame(s, {"method": "ping"})
                    resp = _recv_frame(s)
                    if resp is not None and resp.get("verdict") == "pong":
                        return True
            except OSError:
                pass

    # Cold path: reap stale state, lazy-start, wait for the socket.
    _reap_stale_broker(repo)
    _server_state_dir(repo).mkdir(parents=True, exist_ok=True)
    _spawn_broker(repo)
    return _wait_for_socket(repo, time.time() + _CONNECT_TIMEOUT_SECS)


# --------------------------------------------------------------------------
# lake build fallback (the authoritative-shape result, failure-open path)
# --------------------------------------------------------------------------

def _giant_advice(giant_reason: str) -> str:
    """The worker-facing advisory for a giant-node fallback.

    Which sentence depends on WHICH signal tripped: a line-count giant is
    fixed by splitting; a heartbeat-ceiling giant is fixed by reducing the
    ceiling, and the profiling strategies belong with that case.
    """
    if "maxHeartbeats" in giant_reason:
        return (
            "Working to reduce the maxHeartbeats setting is *strongly "
            "advised* as expensive-to-build nodes are much more cumbersome "
            "to work with and can cause system problems in extreme cases. "
            "Strategies: investigate where the heartbeat budget goes with "
            "`set_option diagnostics true`, or replace a bare `simp` in a "
            "hot block with `simp?`, take its `simp only` suggestion, and "
            "then narrow the expensive step."
        )
    return (
        "Splitting the node / pulling out closed helpers is *strongly "
        "advised* as large nodes are much more cumbersome to work with."
    )


def _giant_phrase(giant_reason: str) -> str:
    """`because ...` clause for a giant fallback, matched to the signal."""
    kind = "expensive" if "maxHeartbeats" in giant_reason else "large"
    return f"the node is too {kind} to warm ({giant_reason})"


def _lake_build_fallback(
    repo: Path, node_name: str, *, reason: str, advice: str = ""
) -> int:
    """Run ``lake build Tablet.NodeName`` and surface its result verbatim.

    This is the failure-open path: whenever the broker/server is unusable,
    ambiguous, or a transitive import changed, the wrapper degrades to exactly
    what the worker would have run anyway. Same blast radius as the worker's
    existing local ``lake build``.
    """
    if advice:
        sys.stderr.write(
            f"incremental-check: falling back to `lake build "
            f"Tablet.{node_name}` because {reason}. {advice}\n"
        )
    else:
        sys.stderr.write(
            f"incremental-check: falling back to `lake build Tablet.{node_name}` "
            f"({reason}).\n"
        )
    sys.stderr.flush()
    proc = subprocess.run(
        ["lake", "build", f"Tablet.{node_name}"],
        cwd=str(repo),
    )
    return proc.returncode


# --------------------------------------------------------------------------
# Supervisor-side active-node prewarm server (preference step 1)
#
# When the supervisor runs the active-node prewarm server (off by default, gated
# by `active_node_prewarm.enabled`), it exports the server's unix socket path to
# the worker burst via `INCREMENTAL_CHECK_ACTIVE_PREWARM_SOCK`. If that env var
# is set AND the socket answers, a check of the ACTIVE node is served from the
# already-warm supervisor server (the cold elaboration was paid at prewarm time)
# — the fast-first-call win. The server itself enforces "active node only": a
# request for any non-active node returns a `fallback` verdict, so the client
# transparently drops to the in-burst broker (step 2).
#
# Fully inert + failure-open: env unset, socket missing/unreachable, or any
# round-trip error -> return None and the caller proceeds to the in-burst
# broker. This step adds NO acceptance path and NO new trust; it is the same
# advisory shape as the broker.
# --------------------------------------------------------------------------

_ACTIVE_PREWARM_SOCK_ENV = "INCREMENTAL_CHECK_ACTIVE_PREWARM_SOCK"


def _active_prewarm_check(
    repo: Path, node_name: str
) -> Optional[Mapping[str, Any]]:
    """Try the supervisor active-node prewarm server. Returns its result dict
    (verdict ok/fail/fallback), or None if it is not configured/reachable (the
    caller then uses the in-burst broker).

    Gap-2 edit-visibility: the worker edits ``Tablet/<node>.lean`` INSIDE its
    own bwrap, so the supervisor server's own workspace copy is the
    last-ACCEPTED text, not the worker's in-flight edit. We therefore read the
    worker's local file and send its TEXT in the frame; the server applies it as
    an LSP ``didChange`` overlay (no disk write) against its warm baseline and
    returns diagnostics for the EDITED text. Without this, the server would
    report a stale verdict for an unrelated buffer.
    """
    sock_raw = os.environ.get(_ACTIVE_PREWARM_SOCK_ENV, "").strip()
    if not sock_raw:
        return None
    sock_path = Path(sock_raw)
    if not sock_path.exists():
        return None
    lean_path = _tablet_lean_path(repo, node_name)
    try:
        text = lean_path.read_text(encoding="utf-8")
    except OSError:
        return None
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(_REQUEST_TIMEOUT_SECS)
            s.connect(str(sock_path))
            _send_frame(
                s, {"method": "check", "node": node_name, "text": text}
            )
            return _recv_frame(s)
    except OSError:
        return None


# --------------------------------------------------------------------------
# Client: connect to broker, send request, translate result
# --------------------------------------------------------------------------

def _broker_check(repo: Path, node_name: str) -> Optional[Mapping[str, Any]]:
    """Send one check request to the broker. Returns the result dict, or None
    if the broker could not be reached / the round-trip failed (caller falls
    back to lake)."""
    if not _ensure_broker(repo):
        return None
    sock_path = _socket_path(repo)
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(_REQUEST_TIMEOUT_SECS)
            s.connect(str(sock_path))
            _send_frame(s, {"method": "check", "node": node_name})
            return _recv_frame(s)
    except OSError:
        return None


def broker_overlay_check(
    repo: Path,
    node_name: str,
    text: str,
    *,
    goal_line: Optional[int] = None,
    goal_column: int = 0,
    max_heartbeats: Optional[int] = None,
) -> Optional[Mapping[str, Any]]:
    """Elaborate ``text`` as an overlay on ``node_name``'s warm broker
    document and return the verdict (optionally carrying the goal state at a
    position). ``None`` when the broker is unreachable — every caller must
    have a path that does not need it.

    ``max_heartbeats`` pins the elaboration budget for THIS request only; the
    broker's default (unbounded) is untouched, so the worker's own
    ``incremental-check`` is unaffected by anything a caller here asks for.

    It writes nothing: the agent's ``Tablet/<node>.lean`` on disk is never
    touched, so a candidate buffer can be compiled while the agent keeps
    editing the same file.

    Sandbox-only, for the same reason ``main`` is: the broker launches a
    BARE ``lake env lean --server`` (correct inside the burst bwrap, which
    already read-only-binds ``.lake/packages``). Started from a host shell
    against a live tablet, that bare ``lake env`` is the manifest
    re-reconciliation that has previously wiped ``.lake/packages``. Off the
    sandbox this returns None and the caller takes its own wrapped-server
    path.
    """
    inside, _detail = _sandbox_markers()
    if not inside and not _allow_unsandboxed():
        return None
    if not _ensure_broker(repo):
        return None
    sock_path = _socket_path(repo)
    request: Dict[str, Any] = {"method": "check", "node": node_name, "text": text}
    if goal_line is not None:
        request["goal"] = {"line": int(goal_line), "column": int(goal_column)}
    if max_heartbeats is not None:
        request["max_heartbeats"] = int(max_heartbeats)
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(_REQUEST_TIMEOUT_SECS)
            s.connect(str(sock_path))
            _send_frame(s, request)
            return _recv_frame(s)
    except OSError:
        return None


def _render_verdict(node_name: str, result: Mapping[str, Any]) -> Optional[int]:
    """Render an ok/fail result to stdout and return the exit code, or None if
    the verdict is fallback/unrecognized (caller drops to the next step)."""
    verdict = result.get("verdict")
    sorry_lines = result.get("sorry", []) or []

    def _emit_sorry_info() -> None:
        # A `sorry` is informative, never a failure: `lake build` reports it as
        # a warning and exits 0, and a proof-formalization node legitimately
        # carries a PERMITTED open `sorry`. Surface its location so a worker
        # aiming to fully close the node still sees where the `sorry` remains.
        for ln in sorry_lines:
            print(f"{ln}  (info: `sorry` remains; advisory only)")

    if verdict == "fail":
        for ln in result.get("lines", []) or []:
            print(ln)
        _emit_sorry_info()
        print(
            f"incremental-check: Tablet.{node_name} has errors (advisory; "
            f"run the deterministic check before submitting)."
        )
        return 1
    if verdict == "ok":
        _emit_sorry_info()
        if sorry_lines:
            print(
                f"incremental-check: Tablet.{node_name} elaborates with no "
                f"errors; {len(sorry_lines)} `sorry` warning(s) remain (advisory "
                f"pre-check; the deterministic worker check is the only sign-off "
                f"and enforces no-sorry-at-submit)."
            )
        else:
            print(
                f"incremental-check: Tablet.{node_name} OK (advisory pre-check; "
                f"the deterministic worker check is the only sign-off)."
            )
        return 0
    return None


def _scan_for_tabs(lean_path: Path, node_name: str) -> Optional[int]:
    """Fast-fail pre-check: Lean files here must not contain tab characters
    (the mathlib text linter forbids them and tabs break whitespace-sensitive
    parsing). Workers occasionally introduce one by accident, which otherwise
    surfaces as a slow, confusing downstream failure. Scan the on-disk file
    and, if any tab is present, emit lake-build-shaped error lines and return a
    nonzero exit code WITHOUT a server round-trip. Returns None when clean.
    """
    try:
        content = lean_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None  # unreadable -> let the normal path report it
    rel = f"Tablet/{node_name}.lean"
    hits = []
    for lineno, line in enumerate(content.splitlines(), start=1):
        col = line.find("\t")
        if col != -1:
            hits.append((lineno, col + 1))
    if not hits:
        return None
    for lineno, col in hits[:20]:
        print(f"{rel}:{lineno}:{col}: error: tab character not allowed; use spaces")
    if len(hits) > 20:
        print(f"{rel}: ... and {len(hits) - 20} more line(s) with tab characters")
    print(
        f"incremental-check: Tablet.{node_name} contains tab characters on "
        f"{len(hits)} line(s); replace every tab with spaces before re-checking."
    )
    return 1


def _run_incremental(repo: Path, node_name: str) -> int:
    lean_path = _tablet_lean_path(repo, node_name)
    if not lean_path.is_file():
        # Mirror lake's "unknown target" by deferring to it; the worker gets
        # the canonical message.
        return _lake_build_fallback(
            repo, node_name, reason=f"Tablet/{node_name}.lean does not exist"
        )

    # Fast-fail on tab characters before any server round-trip: a stray tab is
    # invalid in Lean source here and otherwise produces a slow, confusing
    # failure deep in elaboration or the checker.
    tab_code = _scan_for_tabs(lean_path, node_name)
    if tab_code is not None:
        return tab_code

    # Giant-node skip: a deliberately-huge monolith cannot be warmed reliably,
    # so skip every warm path (active-prewarm server AND in-burst broker) and go
    # straight to `lake build` — the same blast radius as the worker's own
    # build, no wedge risk.
    giant, giant_reason = is_giant_node(lean_path)
    if giant:
        return _lake_build_fallback(
            repo,
            node_name,
            reason=_giant_phrase(giant_reason),
            advice=_giant_advice(giant_reason),
        )

    # Preference step 1: the supervisor active-node prewarm server (fast first
    # call on the active node; inert + failure-open when not configured). The
    # server returns `fallback` for any non-active node, so step 2 covers those.
    prewarm = _active_prewarm_check(repo, node_name)
    if prewarm is not None and prewarm.get("verdict") != "fallback":
        code = _render_verdict(node_name, prewarm)
        if code is not None:
            return code
    # (A `fallback`/unrecognized prewarm verdict, or no prewarm server, falls
    # through to the in-burst broker.)

    # Preference step 2: the in-burst broker (warm across calls within a burst).
    result = _broker_check(repo, node_name)
    if result is None:
        # Preference step 3: lake build.
        return _lake_build_fallback(
            repo, node_name, reason="broker unavailable"
        )

    verdict = result.get("verdict")
    if verdict == "fallback":
        return _lake_build_fallback(
            repo, node_name, reason=str(result.get("reason", "broker requested fallback"))
        )
    code = _render_verdict(node_name, result)
    if code is not None:
        return code
    # Unrecognized verdict -> failure-open.
    return _lake_build_fallback(
        repo, node_name, reason=f"unrecognized broker verdict: {verdict!r}"
    )


def _run_prewarm(repo: Path, node_name: str) -> int:
    """ISSUE-2: pay the cold start at burst BEGIN.

    Lazy-start the broker and drive ONE elaboration of the active node so the
    server is warm before the worker's first edit. Fully failure-open: any
    problem (no broker, no node, server crash, timeout, fallback) is logged
    and we exit 0 — a prewarm must NEVER break the burst, and it does NOT run
    `lake build` (the lazy-start path on the first real check still covers a
    failed prewarm). Unlike a real check it never prints a pass/fail verdict
    or returns nonzero; it is a pure side-effecting warmup.
    """
    try:
        lean_path = _tablet_lean_path(repo, node_name)
        if not lean_path.is_file():
            sys.stderr.write(
                f"incremental-check: prewarm skipped, Tablet/{node_name}.lean "
                f"does not exist.\n"
            )
            return 0
        if "\t" in lean_path.read_text(encoding="utf-8", errors="replace"):
            sys.stderr.write(
                f"incremental-check: prewarm skipped, Tablet/{node_name}.lean "
                f"contains tab characters (the real check will report them).\n"
            )
            return 0
        giant, giant_reason = is_giant_node(lean_path)
        if giant:
            sys.stderr.write(
                f"incremental-check: prewarm skipped because "
                f"{_giant_phrase(giant_reason)}; the real check will "
                f"fall back to lake build. {_giant_advice(giant_reason)}\n"
            )
            return 0
        result = _broker_check(repo, node_name)
        if result is None:
            sys.stderr.write(
                f"incremental-check: prewarm of Tablet.{node_name} could not "
                f"reach the broker (the first real check will cold-start).\n"
            )
            return 0
        verdict = result.get("verdict")
        sys.stderr.write(
            f"incremental-check: prewarmed Tablet.{node_name} "
            f"(broker verdict: {verdict}); server is now warm.\n"
        )
        return 0
    except Exception as exc:  # never let a prewarm break the burst
        sys.stderr.write(
            f"incremental-check: prewarm of Tablet.{node_name} failed "
            f"({type(exc).__name__}: {exc}); ignoring.\n"
        )
        return 0


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------

def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="incremental-check",
        description=(
            "Advisory warm-server Lean pre-check. Green is necessary but not "
            "sufficient; the deterministic worker check is the only sign-off."
        ),
    )
    parser.add_argument(
        "target",
        nargs="?",
        help="Tablet.NodeName (or bare NodeName) to check.",
    )
    parser.add_argument(
        "--repo",
        default=".",
        help="Repo root (defaults to the current directory).",
    )
    parser.add_argument(
        "--shutdown",
        action="store_true",
        help="Reap the per-burst warm-server broker and exit.",
    )
    parser.add_argument(
        "--prewarm",
        action="store_true",
        help=(
            "Lazy-start the broker and elaborate the given node once so the "
            "server is warm before the first edit. Fully failure-open: always "
            "exits 0 and never prints a verdict (a warmup, not a check)."
        ),
    )
    parser.add_argument(
        "--start",
        dest="prewarm",
        action="store_true",
        help=argparse.SUPPRESS,  # alias for --prewarm.
    )
    parser.add_argument(
        "--broker",
        action="store_true",
        help=argparse.SUPPRESS,  # internal: run the detached broker in-process.
    )
    args = parser.parse_args(list(argv) if argv is not None else None)

    repo = Path(args.repo).resolve()

    if args.broker:
        # Internal entry — used only if someone runs the broker directly; the
        # normal lazy-start path calls _broker_main via fork, not this CLI.
        return _broker_main(repo)

    if args.shutdown:
        # Ask a live broker to shut down cleanly, then reap any remnant.
        sock_path = _socket_path(repo)
        if sock_path.exists():
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                    s.settimeout(3.0)
                    s.connect(str(sock_path))
                    _send_frame(s, {"method": "shutdown"})
                    _recv_frame(s)
            except OSError:
                pass
            time.sleep(0.3)
        _reap_stale_broker(repo)
        return 0

    inside, detail = _sandbox_markers()
    if not inside and not _allow_unsandboxed():
        sys.stderr.write(detail + "\n")
        return 2

    if not args.target:
        parser.error("target (Tablet.NodeName) is required")

    try:
        node_name = _parse_target(args.target)
    except ValueError as exc:
        sys.stderr.write(f"incremental-check: {exc}\n")
        # A prewarm must never break the burst, even on a bad node name.
        return 0 if args.prewarm else 2

    if args.prewarm:
        return _run_prewarm(repo, node_name)

    return _run_incremental(repo, node_name)


if __name__ == "__main__":
    raise SystemExit(main())
