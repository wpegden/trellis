"""Isabelle/HOL server lifecycle + TCP protocol client (checker B2a).

This module is the Isabelle analogue of the ``lake``-spawning leaf in
:mod:`trellis.atomic_actions.observations`. It is imported ONLY by the
supervisor-side checker server (:mod:`trellis.checker.server`) and the
Isabelle observation layer (:mod:`trellis.atomic_actions.isabelle_observations`);
nothing on the Lean checker path imports it, so the live Lean checker is
byte-untouched.

Trust boundary
--------------
The raw TCP ``127.0.0.1:<port>`` server + its shared-secret password are
held INTERNALLY by this module. They are bound loopback-only and never
exposed to request-controlled inputs. The AF_UNIX checker server fronts
this module exactly the way it fronts ``lake`` (research report 02): the
repo is derived from the socket's runtime root, never from a request-
supplied path, so hosting Isabelle ops on the same dispatcher preserves
the Lean trust invariants. This module deliberately exposes NO function
that takes a caller-supplied port/password.

Protocol (wire-verified against a live Isabelle2025-2 server — fresh
end-to-end probe 2026-06-19)
------------------------------------------------------------------------
* ``isabelle server -n <name>`` BLOCKS (foreground); we background it,
  read its ``{port, password}`` from ``$ISABELLE_HOME_USER/servers.db``
  (SQLite table ``isabelle_servers(name, port, password)``, mode 0600),
  connect TCP, and send the **password as the first line**.
* Commands are line-delimited ``<command> <JSON-argument>``; the server
  replies with asynchronous messages. A reply is FRAMED one of two ways:
  - a single ``<KIND> <JSON>\\n`` line, OR
  - a line containing only a decimal byte-count ``N`` followed by exactly
    ``N`` bytes of message body (used for long / multi-line messages).
  ``KIND`` is one of ``OK`` / ``ERROR`` / ``FINISHED`` / ``FAILED`` /
  ``NOTE`` (Appendix A.3). ``OK {task}`` opens an async task; the matching
  terminal reply is ``FINISHED``/``FAILED`` carrying the same ``task``.
* ``session_start {"session":"<base>"}`` → ``OK {task}`` → ``NOTE``… →
  ``FINISHED {session_id, tmp_dir, task}`` (the base heap mmap'd warm; the
  Option-B base is ``Tablet_Base``, falling back to ``HOL`` — see
  :func:`resolve_base_session`).
* ``use_theories {session_id, theories:[...], master_dir}`` → ``FINISHED
  {ok, nodes:[{theory_name, node_name, status:{failed, finished, ok, …},
  messages:[{kind, message}], …}]}``. ``ok && status.failed == 0`` is the
  reliable success signal (the ``1+1=3`` negative control returns
  ``ok:false`` + ``failed:1`` + an ``error``-kind message).
* The soundness certificate is read back from the node's ``messages[]``
  (``kind == "writeln"``), emitted by ``thm_oracles``/``thm_deps`` outer
  commands appended to the theory:
  - ``thm_oracles t`` → ``"oracles:"`` then 0+ indented oracle names
    (empty on a clean proof; ``skip_proof`` under ``sorry``).
  - ``thm_deps t`` → ``"dependencies: N"`` then ``N`` indented dep names.

NO ``isabelle process`` (gone in 2025-2 → server / ``ML_process``); NO
``-o proofs=1`` (not a valid option name). Oracle/axiom *names* record
regardless, which is all the sorry/``skip_proof`` soundness check needs.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import socket
import sqlite3
import subprocess
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, List, Mapping, Optional, Sequence, Tuple

# Default path to the Isabelle launcher (the kernel
# ``CheckerDriver::IsabelleHol`` op strings carry no path — the checker
# owns the binary location). Overridable via env for an alternate install.
DEFAULT_ISABELLE_BIN = os.path.expanduser("~/Isabelle2025-2/bin/isabelle")
ISABELLE_BIN_ENV = "TRELLIS_ISABELLE_BIN"

# Phase-0 "Option A" warm-prefix knob: ``headless_consolidate_delay`` at
# ``session_start``. The Isabelle default (2.0) is the source of the ~2s PIDE
# floor; tuning it low (~0.05) collapses a held-open node re-check to sub-0.5s
# with a still-sound verdict. Only applied when the warm-prefix capability is
# enabled (off by default — see ``isabelle_warm_config``); when disabled the
# ``session_start`` wire is byte-identical to before this knob existed.
from trellis.checker.isabelle_warm_config import (
    DEFAULT_CONSOLIDATE_DELAY,
    env_warm_session_enabled,
)

# The warm BASE session the checker's ``session_start`` memory-maps (Option B).
# The B2d scaffold parents the working ``Tablet`` session on a prebuilt
# ``Tablet_Base = HOL-Probability`` heap (warmth flows down the parent edge:
# analysis + probability + ``Complex_Main`` arrive warm). Starting the checker on
# ``Tablet_Base`` (rather than bare ``HOL``) means the checker's elaboration sees
# the same warm surface the worker does. Overridable via env; defaults to
# ``Tablet_Base`` with a graceful fall-back to ``HOL`` when that heap image is
# absent (e.g. the base was not prebuilt yet), so the checker degrades rather
# than hard-failing its ``session_start``.
ISABELLE_BASE_SESSION_ENV = "TRELLIS_ISABELLE_BASE_SESSION"
DEFAULT_BASE_SESSION = "Tablet_Base"
# The universally-present fall-back (the bare HOL heap always ships prebuilt).
FALLBACK_BASE_SESSION = "HOL"

# S7 version pin (B2c-gate hardened design #6). Isabelle 2025-1 accepted a
# bare ``sorry`` (a real soundness bug); the gate hard-pins ≥ 2025-2. The
# release naming is ``Isabelle<YEAR>`` optionally followed by ``-<MINOR>``
# (e.g. ``Isabelle2025``, ``Isabelle2025-1``, ``Isabelle2025-2``); a bare
# ``Isabelle<YEAR>`` is treated as minor 0. Compared as a ``(year, minor)``
# tuple against this floor.
MIN_ISABELLE_VERSION = (2025, 2)
_ISABELLE_VERSION_RE = re.compile(r"Isabelle(\d{4})(?:-(\d+))?")

# Loopback only. The server binds 127.0.0.1; we never connect elsewhere.
LOOPBACK_HOST = "127.0.0.1"

# How long to wait for the backgrounded server to register its row in
# servers.db (it prints the announce line and writes the row within ~1s).
SERVER_REGISTER_TIMEOUT_SECS = 30.0
SERVER_REGISTER_POLL_SECS = 0.1

# Default session-start budget (HOL heap is prebuilt + mmap'd warm, so a
# start is seconds; allow generous slack for a loaded host).
DEFAULT_SESSION_START_TIMEOUT_SECS = 240.0

# Recv chunk size for the framing reader.
_RECV_CHUNK = 65536

# Pure-decimal line => length-prefixed message body of that many bytes.
_LENGTH_PREFIX_RE = re.compile(rb"\A\d+\Z")

# Async-reply kinds the server emits (Appendix A.3). Terminal kinds end an
# async task; ``NOTE`` is progress; ``OK`` opens a task.
_REPLY_KINDS = frozenset({"OK", "ERROR", "FINISHED", "FAILED", "NOTE"})
_TERMINAL_KINDS = frozenset({"FINISHED", "FAILED", "ERROR"})


def isabelle_bin() -> str:
    """Resolve the Isabelle launcher path (env override → pinned default)."""
    raw = os.environ.get(ISABELLE_BIN_ENV, "").strip()
    return raw or DEFAULT_ISABELLE_BIN


def _getenv_isabelle_setting(name: str) -> str:
    """Resolve an Isabelle settings variable (env override → launcher getenv).

    The production checker env does NOT export the Isabelle settings (they live
    in the launcher's ``settings`` script, not the ambient process env), so a
    bare ``os.environ`` read returns empty. Fall back to ``isabelle getenv
    <name>`` (which prints ``<name>=<value>``). Returns ``""`` on any failure so
    the caller degrades rather than raising. Cheap and only on the cold path.
    """
    raw = os.environ.get(name, "").strip()
    if raw:
        return raw
    try:
        proc = subprocess.run(
            [isabelle_bin(), "getenv", name],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    line = (proc.stdout or "").strip()
    if "=" not in line:
        return ""
    return line.split("=", 1)[1].strip()


def _heaps_roots() -> List[Path]:
    """The heap directories that hold built session images.

    A session heap image lands at ``<heaps_root>/<platform>/<Session>``. There
    are two roots: the per-user ``$ISABELLE_HEAPS`` (the default write target)
    and the SHARED ``$ISABELLE_HEAPS_SYSTEM`` (``$ISABELLE_HOME/heaps``, written
    with ``-o system_heaps=true``). The prebuilt
    HOL-Analysis→HOL-Probability→Tablet_Base→Tablet chain lives in the SYSTEM
    heaps, so BOTH must be probed — resolving only the per-user heaps makes the
    warm ``Tablet_Base`` look absent and silently degrades the base to ``HOL``.

    Resolution is via the Isabelle settings (env override → launcher getenv),
    NOT a bare ``os.environ`` read, because the production checker env does not
    export the Isabelle home/heaps variables. Best-effort: an unresolvable root
    is simply skipped.
    """
    roots: List[Path] = []
    seen: set[Path] = set()
    for setting in ("ISABELLE_HEAPS", "ISABELLE_HEAPS_SYSTEM"):
        value = _getenv_isabelle_setting(setting)
        if not value:
            continue
        path = Path(value)
        if path not in seen:
            seen.add(path)
            roots.append(path)
    if not roots:
        # Neither heaps setting resolvable → last-resort per-user heaps from the
        # launcher's home (keeps the cold-start path working).
        try:
            roots.append(_home_user_dir() / "heaps")
        except IsabelleSessionError:
            pass
    return roots


def _session_heap_image_present(session: str) -> bool:
    """True iff a built heap image for ``session`` exists under any heaps root.

    Globs ``<heaps_root>/*/<session>`` across both the user and system heaps
    (the platform subdir is install-specific, e.g.
    ``polyml-5.9.2_x86_64_32-linux``). Used to decide whether the warm
    ``Tablet_Base`` base session is actually available before the checker tries
    to ``session_start`` on it.
    """
    for root in _heaps_roots():
        try:
            if not root.is_dir():
                continue
            for platform_dir in root.iterdir():
                if (platform_dir / session).is_file():
                    return True
        except OSError:
            continue
    return False


def resolve_base_session() -> str:
    """The session the checker should ``session_start`` (Option B warm base).

    Resolution: ``$TRELLIS_ISABELLE_BASE_SESSION`` (operator override) →
    ``Tablet_Base``. If the resolved session's heap image is ABSENT (the base
    was never prebuilt), gracefully degrade to the universally-present ``HOL``
    heap so the checker still starts rather than hard-failing ``session_start``.
    An explicit ``HOL`` override (or the fall-back itself) is returned as-is.
    """
    requested = os.environ.get(ISABELLE_BASE_SESSION_ENV, "").strip() or DEFAULT_BASE_SESSION
    if requested == FALLBACK_BASE_SESSION:
        return requested
    if _session_heap_image_present(requested):
        return requested
    return FALLBACK_BASE_SESSION


def parse_isabelle_version(text: str) -> Optional[Tuple[int, int]]:
    """Parse ``isabelle version`` output into ``(year, minor)``.

    ``"Isabelle2025-2"`` → ``(2025, 2)``; ``"Isabelle2025"`` → ``(2025, 0)``.
    Returns ``None`` when no ``Isabelle<year>`` token is present (an
    unexpected build banner) so the caller fails closed.
    """
    match = _ISABELLE_VERSION_RE.search(text or "")
    if match is None:
        return None
    year = int(match.group(1))
    minor = int(match.group(2)) if match.group(2) is not None else 0
    return (year, minor)


class IsabelleSessionError(RuntimeError):
    """Any failure spawning, connecting to, or driving the Isabelle server.

    Carries a stable ``kind`` so the checker server can map it onto a
    structured transport envelope (mirrors ``CheckerRpcError.kind``):

    - ``spawn_failed``: the ``isabelle server`` process could not be
      started or never registered a ``servers.db`` row.
    - ``connect_failed``: the TCP loopback connect failed.
    - ``protocol_error``: the wire framing or an async reply was malformed
      / unexpected.
    - ``timed_out``: an async command exceeded its budget.
    """

    def __init__(self, kind: str, message: str) -> None:
        super().__init__(message)
        self.kind = kind
        self.message = message


class IsabelleReconcileBudgetExhausted(IsabelleSessionError):
    """The reconcile's shared budget ran out at the PRE-DISPATCH check.

    Raised by :meth:`IsabelleSession.reconcile_accepted_base` when
    ``total_budget_secs`` is exhausted BEFORE the next member promote is
    dispatched — no ``use_theories`` task is in flight, the socket is idle,
    and the session is safe to KEEP (its ``_promoted`` map retains every
    member that finished, so the next reconcile resumes there).

    Deliberately a distinct type (``kind`` stays ``"timed_out"`` for the wire
    envelope): a plain ``timed_out`` raised DURING a member promote means the
    client gave up on an async task the server may STILL be elaborating —
    keeping that session risks a second concurrent Isabelle process on shared
    heaps and a stale-reply socket desync (``_await_terminal`` does not
    correlate task ids), so the warm gate must reap it. The two cases need
    different handling and are only distinguishable by type.
    """


@dataclass
class IsabelleReply:
    """One decoded async reply line/block from the server."""

    kind: str
    payload: Mapping[str, Any] = field(default_factory=dict)
    raw_tail: str = ""


@dataclass
class CheckOutcome:
    """Result of a single ``use_theories`` (+ cert) round-trip.

    Maps onto BOTH the kernel envelopes B2a must synthesize:

    * the ``ExternalCommandObservation`` shape — ``ok``/``failed`` drive
      ``returncode`` (0 iff ``ok and failed == 0``), with ``stdout`` =
      collected ``writeln`` text and ``stderr`` = collected ``error``/
      ``warning`` text;
    * the ``LocalClosureProbeOutput`` cert — ``oracles`` →
      ``oracles_used``, ``dependencies`` → ``kernel_axioms``,
      ``theorem_exists`` ⇐ ``ok and not failed and the writeln carried the
      theorem line``.
    """

    ok: bool
    failed: int
    finished: int
    theory_name: str
    node_name: str
    writeln_lines: List[str] = field(default_factory=list)
    error_lines: List[str] = field(default_factory=list)
    warning_lines: List[str] = field(default_factory=list)
    oracles: List[str] = field(default_factory=list)
    dependencies: List[str] = field(default_factory=list)
    theorem_exists: bool = False
    # B2c-gate Slice 1 (S1/S3): server-extracted from the CHECKER-OWNED probe
    # theory (never from worker-authored cert lines).
    #
    # * ``extra_shyps`` — the residual dangling sort hypotheses from
    #   ``Thm.extra_shyps (Thm.strip_shyps thm)`` (the ``TRELLIS_SHYPS``
    #   writeln). NON-EMPTY ⇒ the theorem rests on an empty/inconsistent type
    #   class (an oracle-BLIND vacuous-``False`` channel); the gate rejects it.
    #   Empty on a clean proof (Appendix B.3 confirmed empty even for fully
    #   generic type-class nodes).
    # * ``statement_repr`` — the elaborated ``Thm.prop_of`` pretty-print
    #   (the ``TRELLIS_STMT`` writeln), NORMALIZED (YXML-stripped, symbol-
    #   spelling canonicalized, whitespace-collapsed). The Correspondence
    #   lane (I1) reuses this same string.
    # * ``statement_hash`` — ``sha256`` of ``statement_repr``; the gate's
    #   record-to-record stability check (NOT equality vs the source hash).
    extra_shyps: List[str] = field(default_factory=list)
    statement_repr: str = ""
    statement_hash: str = ""
    # The fully-qualified fact the certificate actually resolved. A THEOREM node
    # declares a fact named for the node, so this is ``Tablet_<Node>.<node>``. A
    # DEFINITION node declares a CONSTANT — Isabelle names its defining fact
    # ``<node>_def`` and there is no theorem of the bare name — so this is
    # ``Tablet_<Node>.<node>_def``. Recorded (rather than re-derived) so the cert
    # envelope can report the node's real ``root_kind`` instead of assuming
    # "theorem". Empty on a probe that never resolved a principal at all.
    cert_principal: str = ""
    # The CUT-walk attribution (see `_cut_walk_ml`). `residual_oracles` are the
    # oracle occurrences introduced by THIS node's own proof, with any declared
    # boundary child's subproof cut away; `boundary_theorems` are the declared
    # dependencies whose principal theorem the proof actually reached. Together
    # these express "closed relative to its children": residual empty + every
    # boundary a declared child. `oracles` stays the TRANSITIVE union so the two
    # can be cross-checked and so nothing silently loosens during migration.
    # True when `use_theories` never produced a node snapshot for this theory —
    # it failed during import/dependency resolution. Such a reply is a TRANSPORT
    # failure, not a proof verdict, and must not be reported as `invalid_proof`.
    pre_node_failure: bool = False
    residual_oracles: List[str] = field(default_factory=list)
    boundary_theorems: List[str] = field(default_factory=list)
    # The ``Name_Space.names_long`` print of the SAME ``Thm.prop_of`` — cross-
    # node constants carry their defining ``Tablet_<Dep>.`` qualifier. The
    # Correspondence Tier-2 closure axis (I2) parses these qualified names into
    # the sibling-NodeId dependency set. Empty when the probe is pre-this-field
    # or no statement elaborated. NORMALIZED identically to ``statement_repr``.
    statement_repr_long: str = ""
    # ---------------------------------------------------------------- type axis
    # The STRUCTURAL digest input (``TRELLIS_STMT_STRUCT``): a canonical
    # ``ML_Syntax.print_term`` serialization of the theorem's own ``term``,
    # tupled with ``Thm.hyps_of`` and ``Thm.extra_shyps``. Unlike every
    # ``Syntax.string_of_term`` print above, this carries the FULL type and sort
    # annotation at every leaf, because ``Term.typ`` is part of the datatype and
    # nothing prunes it (the printer's ``uncheck`` phase does, by design —
    # ``Doc/Implementation/Syntax.thy:65,198``). NORMALIZED identically to
    # ``statement_repr`` (a near-no-op: the payload is single-line printable
    # ASCII by construction). Empty on a pre-this-field probe.
    statement_type_repr: str = ""
    # ``sha256`` of ``statement_type_repr`` — the corr fingerprint's
    # ``statement_type_hash`` axis. This is the digest that MOVES when a worker
    # generalizes ``nat \<Rightarrow> nat \<Rightarrow> bool`` to
    # ``'a \<Rightarrow> 'a \<Rightarrow> bool`` while the printed term stays
    # byte-identical (the live defect this axis exists for).
    statement_type_hash: str = ""
    # HUMAN-READABLE companion (``TRELLIS_STMT_TYPED``): the same ``Thm.prop_of``
    # printed with ``show_types``/``show_sorts`` (and ``show_abbrevs`` pinned
    # off). DIAGNOSTIC ONLY — never hashed, never gated. Its job is to make the
    # next type-drift incident self-explanatory on screen: the reviewer sees
    # ``R :: nat \<Rightarrow> nat \<Rightarrow> bool`` vs
    # ``R :: 'a \<Rightarrow> 'a \<Rightarrow> bool`` instead of two equal prints
    # and an unexplained hash move. NOT sound as a gate (constants are never
    # type-annotated without ``show_markup``, which the probe's YXML strip
    # removes — ``syntax_phases.ML:665``), which is exactly why it is
    # companion-only.
    statement_repr_typed: str = ""

    @property
    def returncode(self) -> int:
        """0 iff the build reported ok AND no node failed."""
        return 0 if (self.ok and self.failed == 0) else 1

    @property
    def stdout(self) -> str:
        return "\n".join(self.writeln_lines)

    @property
    def stderr(self) -> str:
        return "\n".join(self.error_lines + self.warning_lines)


@dataclass
class WarmCheckOutcome:
    """Structured verdict for one warm-prefix in-flight ``check`` (Phase 1).

    A thin ``{ok, seconds, errors}`` view over the underlying
    :class:`CheckOutcome` (kept whole in ``outcome`` so the cert fields —
    ``oracles``/``dependencies``/``extra_shyps``/``statement_hash`` — are
    reachable). Distinguished from a transport/session failure: a proof error
    yields ``ok=False`` with the error text in ``errors``, never an exception;
    an :class:`IsabelleSessionError` (spawn/connect/protocol/timeout) is the
    anomaly channel and propagates (the dispatcher's cold-backstop trigger in
    Phase 2). ``seconds`` is the wall-clock of the in-flight re-elaboration
    (the warm prefix is not re-elaborated, so this is ~one node).
    """

    ok: bool
    seconds: float
    errors: List[str] = field(default_factory=list)
    outcome: Optional[CheckOutcome] = None


def _home_user_dir() -> Path:
    """Resolve ``$ISABELLE_HOME_USER`` (where ``servers.db`` lives)."""
    raw = os.environ.get("ISABELLE_HOME_USER", "").strip()
    if raw:
        return Path(raw)
    # Not exported in the ambient env → ask the launcher (it prints
    # ``ISABELLE_HOME_USER=<path>``). Cheap (~100ms) and only on the
    # cold-start path; cached by the session object thereafter.
    try:
        proc = subprocess.run(
            [isabelle_bin(), "getenv", "ISABELLE_HOME_USER"],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise IsabelleSessionError(
            "spawn_failed", f"could not resolve ISABELLE_HOME_USER: {exc}"
        )
    line = (proc.stdout or "").strip()
    if "=" not in line:
        raise IsabelleSessionError(
            "spawn_failed",
            f"isabelle getenv ISABELLE_HOME_USER returned no assignment: {line!r}",
        )
    return Path(line.split("=", 1)[1].strip())


def servers_db_path() -> Path:
    """Absolute path to the ``servers.db`` SQLite registry (mode 0600)."""
    return _home_user_dir() / "servers.db"


def _read_server_row(db_path: Path, name: str) -> Optional[Tuple[int, str]]:
    """Return ``(port, password)`` for the named server, or ``None``.

    Best-effort and tolerant of a transient lock / partial write: any
    sqlite error yields ``None`` so the caller's poll loop retries.
    """
    if not db_path.exists():
        return None
    try:
        # read-only, short timeout; the server writes the row once.
        con = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True, timeout=2.0)
    except sqlite3.Error:
        return None
    try:
        cur = con.execute(
            "SELECT port, password FROM isabelle_servers WHERE name = ?", (name,)
        )
        row = cur.fetchone()
    except sqlite3.Error:
        return None
    finally:
        con.close()
    if not row:
        return None
    port, password = row
    if port is None or password is None:
        return None
    try:
        return int(port), str(password)
    except (TypeError, ValueError):
        return None


def _delete_server_row(db_path: Path, name: str) -> None:
    """Delete the named server's ``servers.db`` row (stale-row hygiene).

    Appendix A.5/B.4: the registry keeps STALE rows after a server exits;
    a follow-up connect by the dead port gets ``ECONNREFUSED``. We delete
    BY NAME only, so a co-resident agent's row is never disturbed.
    """
    if not db_path.exists():
        return
    try:
        con = sqlite3.connect(db_path, timeout=5.0)
    except sqlite3.Error:
        return
    try:
        con.execute("DELETE FROM isabelle_servers WHERE name = ?", (name,))
        con.commit()
    except sqlite3.Error:
        pass
    finally:
        con.close()


class IsabelleSession:
    """A single live Isabelle server + HOL session, fronted internally.

    Lifecycle::

        sess = IsabelleSession(name="trellis-isabelle-<runtime>")
        sess.start()                       # spawn server + session_start HOL
        outcome = sess.check_theory(...)    # use_theories + cert
        sess.close()                        # reap server BY NAME

    The instance is NOT thread-safe on its own socket; the checker server
    serializes Isabelle ops under its existing ``_workspace_lock`` exactly
    as it serializes ``lake`` (one server, one warm HOL session per
    runtime). A module-level ``_lock`` guards spawn/reap so two sessions
    with the same name can't race on the registry.
    """

    _spawn_lock = threading.Lock()

    def __init__(
        self,
        *,
        name: str,
        isabelle_bin_path: Optional[str] = None,
        session: Optional[str] = None,
        session_dirs: Optional[Sequence[str]] = None,
        start_timeout_secs: float = DEFAULT_SESSION_START_TIMEOUT_SECS,
        warm_prefix_enabled: Optional[bool] = None,
        consolidate_delay: float = DEFAULT_CONSOLIDATE_DELAY,
    ) -> None:
        if not name or not re.fullmatch(r"[A-Za-z0-9_.\-]+", name):
            raise IsabelleSessionError(
                "spawn_failed",
                f"invalid Isabelle server name {name!r} (must match [A-Za-z0-9_.-]+)",
            )
        self.name = name
        # ``session=None`` (the steady state) resolves the warm Option-B base
        # (``Tablet_Base``, env-overridable) with a graceful fall-back to ``HOL``
        # when that heap image is absent. An explicit caller-supplied session
        # (e.g. a test pinning ``HOL``) is honored verbatim.
        self.session = session if session is not None else resolve_base_session()
        # The session-root search dirs (``dirs`` on the ``session_start`` wire).
        # The scaffold ``Tablet_Base``/``Tablet`` sessions are DEFINED only in the
        # per-tablet scaffold ROOT, not in any directory the server scans by
        # default — without this the server reports ``Undefined session(s):
        # "Tablet_Base"``. The universal ``HOL`` fall-back base is built in and
        # needs no dir. A copy keeps the instance immutable to caller mutation.
        self.session_dirs: List[str] = list(session_dirs or [])
        self.start_timeout_secs = float(start_timeout_secs)
        # Phase-1 warm-prefix capability (Option A). OFF by default: when
        # ``warm_prefix_enabled`` is left unset it resolves from the env switch
        # (``TRELLIS_ISABELLE_WARM_SESSION``, default False). When OFF the
        # session behaves EXACTLY as before — ``_session_start`` sends no
        # ``options`` (byte-identical wire) and the warm methods
        # (``promote``/``evict``/``reconcile_accepted_base``/``check_node``)
        # raise rather than silently no-op, so a miswire is loud. The
        # ``consolidate_delay`` is only sent when the capability is ON.
        self.warm_prefix_enabled = (
            env_warm_session_enabled()
            if warm_prefix_enabled is None
            else bool(warm_prefix_enabled)
        )
        self.consolidate_delay = float(consolidate_delay)
        self._bin = isabelle_bin_path or isabelle_bin()
        self._db_path = servers_db_path()
        self._proc: Optional[subprocess.Popen[str]] = None
        self._sock: Optional[socket.socket] = None
        self._buf = bytearray()
        self._session_id: Optional[str] = None
        self._port: Optional[int] = None
        # The warm prefix: theory-name → sha256 of the bytes promoted. Only
        # populated when ``warm_prefix_enabled``; empty (and untouched) on the
        # legacy cold path. A node is warm in the prefix ONLY because it was
        # promoted from the synced accepted set, never because a check went
        # green (the design's "sync-coherent, not advisory-green" invariant).
        self._promoted: dict[str, str] = {}
        # H1 (warm-residency-staleness): in-flight node theory-name → its CURRENT
        # content-keyed alias theory name (``<node>__In_<sha12>``). DISTINCT from
        # ``_promoted`` (accepted siblings): the in-flight node never enters
        # ``_promoted`` (it is excluded from the accepted set while active). A
        # held-open document retains the LAST loaded version of a theory under a
        # REUSED name, so a worker proof-REPLACING edit between bursts
        # (clean→``sorry``) can read residency-STALE; and a ``purge_theories`` +
        # re-``use_theories`` of the same name CORRUPTS the held-open document
        # ("Illegal theory header", live-confirmed on 2025-2), so a purge-before
        # is not viable. Instead each in-flight check elaborates under a fresh
        # content-keyed alias name (see :meth:`fresh_inflight_theory`): changed
        # bytes ⇒ new sha ⇒ new alias ⇒ guaranteed-fresh elaboration; unchanged
        # bytes ⇒ same alias, reused warm with no re-elaboration. Empty +
        # untouched on the cold path.
        self._inflight_alias: dict[str, str] = {}

    # ------------------------------ lifecycle ------------------------------

    @property
    def session_id(self) -> Optional[str]:
        return self._session_id

    def _assert_version_pin(self) -> None:
        """S7: hard-pin the Isabelle version ≥ 2025-2 before any spawn.

        Runs ``isabelle version`` and parses ``Isabelle<year>-<minor>``.
        Raises ``IsabelleSessionError("spawn_failed", …)`` when the binary
        cannot be queried, the banner is unparseable, or the version is
        below the floor (2025-1 accepted ``sorry`` — a real soundness bug).
        """
        try:
            proc = subprocess.run(
                [self._bin, "version"],
                capture_output=True,
                text=True,
                timeout=60,
            )
        except (OSError, subprocess.SubprocessError) as exc:
            raise IsabelleSessionError(
                "spawn_failed", f"could not run 'isabelle version': {exc}"
            )
        banner = ((proc.stdout or "") + "\n" + (proc.stderr or "")).strip()
        version = parse_isabelle_version(banner)
        if version is None:
            raise IsabelleSessionError(
                "spawn_failed",
                f"could not parse Isabelle version from {banner!r}",
            )
        if version < MIN_ISABELLE_VERSION:
            raise IsabelleSessionError(
                "spawn_failed",
                f"Isabelle version {version[0]}-{version[1]} is below the "
                f"required floor {MIN_ISABELLE_VERSION[0]}-"
                f"{MIN_ISABELLE_VERSION[1]} (2025-1 accepted 'sorry'); "
                f"refusing to start",
            )

    def start(self) -> None:
        """Spawn the server, connect, authenticate, and start the HOL session."""
        # S7 version pin FIRST: refuse to spawn anything below 2025-2.
        self._assert_version_pin()
        with IsabelleSession._spawn_lock:
            # Stale-row hygiene: a prior crashed run may have left a dead
            # row under our name. Reap by name first (no-op if absent), so
            # we don't connect to a dead port.
            _delete_server_row(self._db_path, self.name)
            self._spawn_server()
            port, password = self._await_registration()
        self._connect_and_auth(port, password)
        self._session_id = self._session_start()

    def _spawn_server(self) -> None:
        try:
            # ``isabelle server`` blocks in the foreground → background it.
            # Capture stdout so we can drain the announce line without it
            # filling a pipe; the authoritative {port,password} comes from
            # servers.db regardless.
            self._proc = subprocess.Popen(
                [self._bin, "server", "-n", self.name],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
        except OSError as exc:
            raise IsabelleSessionError(
                "spawn_failed", f"could not spawn isabelle server: {exc}"
            )

    def _await_registration(self) -> Tuple[int, str]:
        deadline = time.monotonic() + SERVER_REGISTER_TIMEOUT_SECS
        while time.monotonic() < deadline:
            # If the process died before registering, surface its output.
            if self._proc is not None and self._proc.poll() is not None:
                tail = ""
                if self._proc.stdout is not None:
                    try:
                        tail = self._proc.stdout.read() or ""
                    except OSError:
                        tail = ""
                raise IsabelleSessionError(
                    "spawn_failed",
                    f"isabelle server exited before registering "
                    f"(rc={self._proc.returncode}): {tail.strip()[:400]}",
                )
            row = _read_server_row(self._db_path, self.name)
            if row is not None:
                self._port = row[0]
                return row
            time.sleep(SERVER_REGISTER_POLL_SECS)
        raise IsabelleSessionError(
            "spawn_failed",
            f"isabelle server '{self.name}' did not register a servers.db row "
            f"within {SERVER_REGISTER_TIMEOUT_SECS}s",
        )

    def _connect_and_auth(self, port: int, password: str) -> None:
        try:
            sock = socket.create_connection(
                (LOOPBACK_HOST, int(port)), timeout=30.0
            )
        except OSError as exc:
            raise IsabelleSessionError(
                "connect_failed",
                f"could not connect to isabelle server: {exc}",
            )
        sock.settimeout(self.start_timeout_secs)
        self._sock = sock
        # Password as the FIRST line (Appendix A.3).
        try:
            sock.sendall((password + "\n").encode("utf-8"))
        except OSError as exc:
            raise IsabelleSessionError(
                "connect_failed", f"failed to send password: {exc}"
            )

    def _session_start(self) -> str:
        # Flag OFF + ``HOL`` base (the test-pinned legacy case) → the wire is
        # byte-identical to before the warm-prefix capability existed:
        # ``{"session": "HOL"}`` with no ``options``/``dirs``. Flag ON → tune
        # ``headless_consolidate_delay`` low (Phase-0 Option A: the single knob
        # that turns the ~2s PIDE floor into a sub-0.5s node re-check) AND pin
        # ``quick_and_dirty=false`` to match the option the cold ``isabelle
        # build`` runs under (the ROOT options set it), so warm and cold
        # elaborate with the same option state. NOTE (run-disproven earlier
        # claim): ``quick_and_dirty=false`` does NOT make the warm PIDE
        # ``use_theories`` REJECT a ``sorry`` at load — headless PIDE admits
        # the skip-proof step regardless, the node loads green, and the
        # ``sorry`` surfaces as the ``skip_proof`` oracle in the ``thm_deps``
        # cert (which is where the soundness gate catches it; open-node
        # detection is the kernel's textual ``is_node_open``, not a load
        # failure). The pin is kept for warm==cold OPTION parity, not as a
        # ``sorry`` gate.
        # The ``options`` list carries ``key=value`` strings the server parses.
        args: dict[str, Any] = {"session": self.session}
        options: List[str] = []
        # A scaffold base (``Tablet_Base``/``Tablet``, anything but the built-in
        # ``HOL`` fall-back) is DEFINED only in the per-tablet scaffold ROOT and
        # BUILT only in the shared SYSTEM heaps. So when starting on such a base
        # we MUST (a) point the server at the scaffold dir(s) via ``dirs`` (else
        # ``Undefined session(s)``), and (b) set ``system_heaps=true`` so the
        # server resolves the prebuilt base image from the SYSTEM heaps and does
        # NOT rebuild the HOL-Analysis→…→Tablet_Base chain into the per-user heaps
        # (the host-saturating rebuild). This matches the cold ``isabelle build``,
        # which builds + reads the SAME system heaps — so cold and warm agree on
        # one image set. The ``HOL`` fall-back is built in + universally prebuilt,
        # so it needs neither (preserving the byte-identical legacy wire).
        if self.session != FALLBACK_BASE_SESSION:
            if self.session_dirs:
                args["dirs"] = list(self.session_dirs)
            options.append("system_heaps=true")
        if self.warm_prefix_enabled:
            options.extend(
                [
                    f"headless_consolidate_delay={self.consolidate_delay}",
                    "quick_and_dirty=false",
                ]
            )
        if options:
            args["options"] = options
        self._send("session_start", args)
        reply = self._await_terminal(timeout_secs=self.start_timeout_secs)
        if reply.kind != "FINISHED":
            raise IsabelleSessionError(
                "protocol_error",
                f"session_start did not FINISH (got {reply.kind}): "
                f"{reply.raw_tail[:300]}",
            )
        sid = reply.payload.get("session_id")
        if not isinstance(sid, str) or not sid:
            raise IsabelleSessionError(
                "protocol_error",
                f"session_start FINISHED without a session_id: {reply.payload}",
            )
        return sid

    def close(self) -> None:
        """Reap the server BY NAME and clean its registry row.

        Idempotent and best-effort: safe to call from a ``finally`` even
        if ``start`` failed partway. We send ``session_stop`` if a session
        is live, close the socket, then ``isabelle server -n <name> -x``
        (the documented by-name stop) and delete the registry row so a
        later same-name reuse doesn't hit a dead port.
        """
        if self._sock is not None and self._session_id is not None:
            try:
                self._send("session_stop", {"session_id": self._session_id})
                # Drain its terminal reply briefly; ignore failures.
                self._await_terminal(timeout_secs=15.0)
            except (IsabelleSessionError, OSError):
                pass
        if self._sock is not None:
            try:
                self._sock.close()
            except OSError:
                pass
            self._sock = None
        # Stop the named server explicitly (the announce process will exit).
        try:
            subprocess.run(
                [self._bin, "server", "-n", self.name, "-x"],
                capture_output=True,
                text=True,
                timeout=30,
            )
        except (OSError, subprocess.SubprocessError):
            pass
        if self._proc is not None:
            try:
                self._proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                try:
                    self._proc.kill()
                except OSError:
                    pass
            except OSError:
                pass
            if self._proc.stdout is not None:
                try:
                    self._proc.stdout.close()
                except OSError:
                    pass
            self._proc = None
        _delete_server_row(self._db_path, self.name)
        self._session_id = None

    def __enter__(self) -> "IsabelleSession":
        self.start()
        return self

    def __exit__(self, *_exc: Any) -> None:
        self.close()

    # ------------------------------ commands ------------------------------

    def check_theory(
        self,
        *,
        master_dir: str,
        theory: str,
        cert_theorem: Optional[str] = None,
        timeout_secs: float = DEFAULT_SESSION_START_TIMEOUT_SECS,
    ) -> CheckOutcome:
        """Run ``use_theories`` on a single ``theory`` and parse the result.

        The theory file must already exist at ``<master_dir>/<theory>.thy``;
        appending ``thm_oracles``/``thm_deps`` for ``cert_theorem`` (if the
        theory does NOT already carry them) is the caller's responsibility —
        :func:`write_theory_with_cert` builds that file. This method does
        not write any file; it only drives the wire.

        In-flight FRESHNESS (H1) is the CALLER's responsibility on this method:
        ``check_theory`` drives ``use_theories`` for exactly the named ``theory``
        (so the cold one-shot path stays byte-identical). The warm cert path
        (:func:`run_cert_probe`) builds the worker node via
        :meth:`fresh_inflight_theory` and points the probe at the resulting
        content-keyed alias, so a held-open worker edit re-elaborates fresh; the
        warm advisory (:meth:`check_node`) does the same.
        """
        if self._sock is None or self._session_id is None:
            raise IsabelleSessionError(
                "protocol_error", "check_theory called before start()"
            )
        return self._use_theories_outcome(
            theory=theory,
            master_dir=master_dir,
            cert_theorem=cert_theorem,
            timeout_secs=timeout_secs,
        )

    def _use_theories_outcome(
        self,
        *,
        theory: str,
        master_dir: str,
        cert_theorem: Optional[str],
        timeout_secs: float,
    ) -> CheckOutcome:
        """Drive one ``use_theories`` for ``theory`` and parse the result.

        The single point that frames + awaits + parses a ``use_theories``
        round-trip, shared by the cold :meth:`check_theory` and the warm
        :meth:`check_node`. The wire args are IDENTICAL on both paths
        (``session_id`` + single ``theories`` + ``master_dir``); the only
        difference between cold and warm is whether the session was started
        with a held-open warm prefix already loaded — which is invisible to
        this method, so the cold path is byte-identical to before this refactor.
        """
        self._send(
            "use_theories",
            {
                "session_id": self._session_id,
                "theories": [theory],
                "master_dir": master_dir,
            },
        )
        reply = self._await_terminal(timeout_secs=timeout_secs)
        return self._parse_use_theories_reply(reply, theory, cert_theorem)

    # ----------------------- warm-prefix capability (Phase 1) -----------------------
    #
    # OFF by default (``warm_prefix_enabled``). Phase 1 adds the capability but
    # does NOT route the node-check gate through it (that is Phase 2); these
    # methods are dormant unless a caller (today: the tests) opts in. They model
    # the Phase-0 Option-A semantics on the held-open ``isabelle server``:
    # promote = ``use_theories`` an accepted sibling into the warm document (it
    # persists, idempotent); check = ``use_theories`` ONLY the in-flight theory
    # against the warm prefix (sub-second, prefix not re-elaborated); evict =
    # ``purge_theories`` a stale version so a re-promote re-elaborates fresh with
    # no residue.

    def _require_warm(self, op: str) -> None:
        if not self.warm_prefix_enabled:
            raise IsabelleSessionError(
                "protocol_error",
                f"{op} requires warm_prefix_enabled (capability is off)",
            )
        if self._sock is None or self._session_id is None:
            raise IsabelleSessionError(
                "protocol_error", f"{op} called before start()"
            )

    @property
    def promoted_theories(self) -> List[str]:
        """The theory names currently warm in the prefix (sorted, a copy)."""
        return sorted(self._promoted)

    @staticmethod
    def _theory_sha(master_dir: str, theory: str) -> Optional[str]:
        """sha256 of ``<master_dir>/<theory>.thy``; None if it is absent."""
        path = Path(master_dir) / f"{theory}.thy"
        try:
            return hashlib.sha256(path.read_bytes()).hexdigest()
        except OSError:
            return None

    def promote(
        self,
        *,
        master_dir: str,
        theory: str,
        timeout_secs: float = DEFAULT_SESSION_START_TIMEOUT_SECS,
    ) -> CheckOutcome:
        """Load an accepted node ``.thy`` into the warm prefix (Option A).

        ``use_theories`` the sibling on the held-open session so it persists
        WARM for later checks (the server retains it until purged). The bytes'
        sha256 is recorded so :meth:`reconcile_accepted_base` can detect a
        later content change. The sibling's own ``imports`` must already be
        warm (promote in dependency order — see
        :meth:`reconcile_accepted_base`); ``use_theories`` resolves any
        not-yet-warm import from ``master_dir`` source, so an out-of-order
        promote still elaborates correctly, just less warmly.

        Returns the :class:`CheckOutcome`; the caller may inspect ``ok`` to
        confirm the sibling elaborated (an accepted sibling always should).
        Raises :class:`IsabelleSessionError` if the capability is off or the
        session is not started.
        """
        self._require_warm("promote")
        outcome = self._use_theories_outcome(
            theory=theory,
            master_dir=master_dir,
            cert_theorem=None,
            timeout_secs=timeout_secs,
        )
        # Record the promotion ONLY if the sibling actually elaborated. Recording
        # a FAILED promote makes `reconcile_accepted_base` treat the theory as
        # warm-and-current, so it never retries it — the broken state then
        # persists for every later check of every node whose cone touches it,
        # turning a transient failure into a stable one. (Observed: a corrupted
        # theory kept a batch of correct nodes unacceptable across repeated
        # rounds precisely because the bad promote had been recorded.)
        sha = self._theory_sha(master_dir, theory)
        if sha is not None and outcome.ok and outcome.failed == 0:
            self._promoted[theory] = sha
        return outcome

    def evict(
        self,
        *,
        master_dir: str,
        theory: str,
        timeout_secs: float = 60.0,
    ) -> None:
        """Drop a stale node version from the warm prefix with no residue.

        ``purge_theories`` the theory off the held-open session (the documented
        by-name removal) and forget its sha, so a subsequent :meth:`promote` of
        the edited bytes re-elaborates fresh — the warm state then equals a cold
        build of the same accepted set (no half-loaded residue, no shadowing of
        a later node). Idempotent and best-effort on the wire: a purge of a
        not-resident theory is a no-op. Raises only on the capability/lifecycle
        guard.
        """
        self._require_warm("evict")
        self._promoted.pop(theory, None)
        self._purge_theories([theory], master_dir=master_dir, timeout_secs=timeout_secs)

    def _purge_theories(
        self,
        theories: Sequence[str],
        *,
        master_dir: str,
        timeout_secs: float,
    ) -> None:
        """Wire ``purge_theories {session_id, theories, master_dir}``.

        ``purge_theories`` is a SYNCHRONOUS server command (Isabelle
        ``server_commands.scala::Purge_Theories`` returns a plain result, so
        ``server.scala`` answers with a single ``OK {purged, retained}`` and
        NO async task / ``FINISHED`` — unlike ``use_theories``/``session_start``,
        which open a task and stream ``OK {task}`` … ``FINISHED``). So we read
        ONE synchronous reply (``OK``, terminal here; ``ERROR`` on a bad
        session) rather than awaiting a ``FINISHED`` that never comes (which
        would block until the read timeout — live-confirmed 2026-06-23).

        Best-effort: a theory that was not resident is simply absent from
        ``purged``; a non-purge is not an error (eviction is idempotent). An
        ``ERROR`` reply is swallowed (the purge is advisory residue cleanup —
        a failure to purge leaves the prior version resident, which the next
        re-promote / cold cross-check still corrects), but the reply IS drained
        so the socket stays in sync.
        """
        if not theories:
            return
        self._send(
            "purge_theories",
            {
                "session_id": self._session_id,
                "theories": list(theories),
                "master_dir": master_dir,
            },
        )
        # Drain the single synchronous reply so the socket stays in sync.
        self._await_sync_reply(timeout_secs=timeout_secs)

    def _await_sync_reply(self, *, timeout_secs: float) -> IsabelleReply:
        """Read ONE reply for a SYNCHRONOUS server command (``OK``/``ERROR``).

        A synchronous command (``purge_theories``) answers with a single
        ``OK <result>`` (or ``ERROR``) and opens no async task, so ``OK`` is
        terminal — unlike :meth:`_await_terminal`, which skips ``OK`` waiting
        for ``FINISHED`` (the task-completion reply async commands stream). A
        ``NOTE`` (progress) is still skipped. Raises ``timed_out`` if no reply
        arrives, ``protocol_error`` on EOF.
        """
        deadline = time.monotonic() + max(1.0, float(timeout_secs))
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise IsabelleSessionError(
                    "timed_out",
                    f"no synchronous reply within {timeout_secs}s",
                )
            if self._sock is not None:
                self._sock.settimeout(remaining)
            reply = self._read_reply()
            if reply is None:
                raise IsabelleSessionError(
                    "protocol_error", "server closed the connection unexpectedly"
                )
            if reply.kind == "NOTE":
                continue  # progress; keep reading
            return reply  # OK (terminal here) / ERROR / FINISHED / FAILED

    def purge_inflight(
        self,
        *,
        master_dir: str,
        theory: str,
        timeout_secs: float = 60.0,
    ) -> None:
        """Drop the in-flight node from the held-open warm document.

        Hardening H1: the in-flight node is never promoted (it enters the warm
        prefix only once accepted, via :meth:`promote`), so it is not in
        ``self._promoted`` and :meth:`reconcile_accepted_base` never re-elaborates
        it. A held-open ``isabelle server`` document retains the LAST loaded
        version of a theory; a worker that re-checks the SAME node after a
        proof-REPLACING edit (clean→``sorry``, a body swap) must re-elaborate
        FRESH, not see a residency-stale verdict. ``purge_theories`` the in-flight
        theory before the re-check so the next ``use_theories`` re-reads the
        current disk bytes from scratch — explicit freshness rather than relying
        on PIDE's content-version tracking (design §3.3 / isa_dev.sh ``cmd_check``).

        We purge ONLY the node, not a fixed-name ``__Cert`` probe: the cert probe
        uses a per-call UNIQUE name (:func:`cert_probe_theory_name`'s ``nonce``)
        and is NOT purged off the document (purging a probe that ``imports`` the
        node also invalidates the node — live-confirmed), so there is no
        fixed-name probe to drop here. Idempotent + best-effort on the wire (a
        purge of a non-resident theory is a no-op); does NOT touch
        ``self._promoted``. Raises only the capability/lifecycle guard.
        """
        self._require_warm("purge_inflight")
        self._purge_theories(
            [theory], master_dir=master_dir, timeout_secs=timeout_secs
        )

    def fresh_inflight_theory(
        self,
        *,
        master_dir: str,
        theory: str,
    ) -> str:
        """Content-keyed fresh-elaboration name for the IN-FLIGHT ``theory`` (H1).

        Returns the theory NAME ``check_node``/the cert path must drive
        ``use_theories`` against so it elaborates the CURRENT on-disk bytes — not
        a residency-stale prior version. The mechanism is the same one the cert
        probe already uses to defeat held-open staleness: a UNIQUE theory name
        forces a fresh elaboration (PIDE retains the LAST loaded version under a
        REUSED name, and — live-confirmed on Isabelle 2025-2 — a
        ``purge_theories`` + re-``use_theories`` of the SAME name CORRUPTS the
        held-open document with "Illegal theory header", so a targeted
        purge-before is NOT viable; a fresh name is). The name is keyed on the
        content sha256, which makes it the H1 content-GATE simultaneously:

        * bytes CHANGED since the last in-flight check (clean→``sorry``, a body
          swap) ⇒ a NEW sha ⇒ a NEW alias name ⇒ a guaranteed-fresh
          elaboration that reflects the current content (the live ``Subcritical``
          stale-clean read can no longer occur);
        * bytes UNCHANGED ⇒ the SAME sha ⇒ the SAME alias name, already resident
          warm ⇒ PIDE reuses it with no needless re-elaboration.

        Implementation: copy ``<master_dir>/<theory>.thy`` to a sibling
        ``<theory>__In_<sha12>.thy`` with ONLY its ``theory`` header line
        renamed to match (the body — imports + proof + any co-located cert
        commands referencing the principal by its UNQUALIFIED name — is
        byte-identical, so it elaborates identically). The ``__In`` infix marks
        it a checker-owned in-flight alias; like the ``__Cert`` probe it is
        swept by the scaffold's ``*__In*`` / ``*__Cert*`` enumeration guards and
        never enters the accepted set. The alias ``.thy`` is the caller's to
        clean up after the cert is read (mirrors the probe's
        ``probe_path.unlink()``).

        Warm-only: on a cold/flag-OFF session there is no held-open residency to
        stale, so this returns ``theory`` UNCHANGED and writes no alias (the cold
        wire is byte-identical to before H1). A missing source file also returns
        ``theory`` unchanged (the elaboration then fails loudly on the absent
        file, exactly as before).
        """
        if not self.warm_prefix_enabled:
            return theory
        src = Path(master_dir) / f"{theory}.thy"
        try:
            raw = src.read_bytes()
        except OSError:
            return theory
        sha12 = hashlib.sha256(raw).hexdigest()[:12]
        alias = f"{theory}__In_{sha12}"
        alias_path = Path(master_dir) / f"{alias}.thy"
        # Re-write only when absent (same content ⇒ same name ⇒ already on disk
        # and warm-resident; no needless rewrite/re-elaboration).
        if not alias_path.exists():
            text = raw.decode("utf-8", errors="replace")
            # Rename ONLY the leading ``theory <name>`` header to the alias; the
            # rest (imports/body/cert) is untouched so it elaborates identically.
            renamed = re.sub(
                rf"(\btheory\s+){re.escape(theory)}\b",
                rf"\g<1>{alias}",
                text,
                count=1,
            )
            try:
                alias_path.write_text(renamed, encoding="utf-8")
            except OSError:
                # Cannot stage the alias → fall back to the original name (the
                # check still runs, just without the freshness guarantee).
                return theory
        # A content change supersedes the prior alias for this node: delete that
        # now-stale alias FILE so superseded copies do not accumulate in the
        # session dir (it can never re-enter a build once unlinked; the warm
        # document keeps the already-loaded copy resident, harmless, until the
        # session is rebuilt cold). We do NOT ``purge_theories`` it (that would
        # corrupt the held-open document); leaving it resident is fine. The live
        # alias file is kept on disk so an unchanged re-check reuses it.
        prior = self._inflight_alias.get(theory)
        if prior is not None and prior != alias:
            try:
                (Path(master_dir) / f"{prior}.thy").unlink()
            except OSError:
                pass
        self._inflight_alias[theory] = alias
        return alias

    @property
    def inflight_alias_of(self) -> "dict[str, str]":
        """node theory → its current content-keyed in-flight alias (a copy)."""
        return dict(self._inflight_alias)

    def check_node(
        self,
        *,
        master_dir: str,
        theory: str,
        cert_theorem: Optional[str] = None,
        timeout_secs: float = DEFAULT_SESSION_START_TIMEOUT_SECS,
    ) -> WarmCheckOutcome:
        """(Re-)elaborate ONE in-flight ``theory`` against the warm prefix.

        ``use_theories`` only the in-flight theory on the held-open session: the
        warm Preamble + accepted-sibling prefix is reused (not re-elaborated),
        so this pays ~one node. Returns a structured :class:`WarmCheckOutcome`
        (``ok``/``seconds``/``errors`` + the full underlying
        :class:`CheckOutcome` with the cert fields). The in-flight theory is NOT
        added to the warm prefix — it enters the prefix only via :meth:`promote`
        once accepted — so a failed check leaves no residue.

        A proof error is ``ok=False`` (never an exception); an
        :class:`IsabelleSessionError` (transport/session anomaly) propagates.
        """
        self._require_warm("check_node")
        # H1: elaborate the in-flight node under a fresh CONTENT-KEYED alias so a
        # worker proof-REPLACING edit (clean→``sorry``) re-elaborates fresh
        # rather than reading the residency-stale prior version; unchanged bytes
        # reuse the same warm-resident alias (no re-elaboration). The outcome is
        # relabeled below to the caller's ``theory`` so the alias is invisible.
        elaborate = self.fresh_inflight_theory(master_dir=master_dir, theory=theory)
        t0 = time.monotonic()
        outcome = self._use_theories_outcome(
            theory=elaborate,
            master_dir=master_dir,
            cert_theorem=cert_theorem,
            timeout_secs=timeout_secs,
        )
        seconds = time.monotonic() - t0
        if elaborate != theory:
            outcome.theory_name = theory
        return WarmCheckOutcome(
            ok=(outcome.returncode == 0),
            seconds=seconds,
            errors=list(outcome.error_lines),
            outcome=outcome,
        )

    def reconcile_accepted_base(
        self,
        *,
        master_dir: str,
        accepted: Sequence[str],
        timeout_secs: float = DEFAULT_SESSION_START_TIMEOUT_SECS,
        total_budget_secs: Optional[float] = None,
    ) -> None:
        """Hold the caller's REQUIRED theory set warm (content-hashed).

        ``accepted`` is the ordered (dependency-respecting) list of theories the
        in-flight node actually needs — its transitive import cone, computed by
        :func:`trellis.checker.isabelle_warm_gate._import_cone_theories`. It is
        NOT "every node theory on disk": the projected files are whatever the
        worker most recently authored, accepted or not, and naming a sibling the
        node does not import lets that sibling's failure reach this node's
        verdict. Mirrors the Lean checker's sync-invalidated olean cache, whose
        unit is likewise the node's import closure (``lake build Tablet.<Node>``).

        * a theory in ``accepted`` but absent/stale in the warm prefix (sha
          differs) → promote the current bytes (``use_theories``);
        * a promoted theory NOT in ``accepted`` → left alone; it is unreachable
          from this node and evicting it would only cost a re-elaboration later.

        Budgets: ``timeout_secs`` is the legacy PER-PROMOTE budget (each member
        ``use_theories`` may take up to that long). ``total_budget_secs``, when
        given, is a SHARED budget across the whole reconcile — the caller's op
        budget threaded down by the warm gate: each promote gets the REMAINING
        time (never multiplying per member), and exhausting the budget BEFORE a
        needed promote is dispatched raises
        :class:`IsabelleReconcileBudgetExhausted` (kind ``timed_out``; no task
        in flight — the session is safe to keep). A member promote that itself
        times out CLIENT-SIDE raises the plain ``timed_out`` base class: the
        server may still be elaborating that task, so the caller must not keep
        the session. Already-promoted members stay recorded in
        ``self._promoted`` (a promote is recorded when it finishes), so a later
        reconcile of the SAME session resumes where this one ran out rather
        than redoing the finished work.

        This is the ONLY way content enters the warm base; there is no
        promote-on-green path. Promotion runs in ``accepted`` order so a
        sibling is promoted only after its own imports (earlier in the list) are
        warm. Raises :class:`IsabelleSessionError` if the capability is off or
        the session is not started.
        """
        self._require_warm("reconcile_accepted_base")
        deadline: Optional[float] = None
        if total_budget_secs is not None:
            deadline = time.monotonic() + max(0.0, float(total_budget_secs))
        # Promote anything in the cone that is absent or content-changed, in
        # dependency order. Theories OUTSIDE the cone are deliberately left
        # resident and untouched: a theory that is not imported cannot affect
        # this node's elaboration, and dropping it would only force a needless
        # re-elaboration the next time some node does import it. This mirrors
        # Lean, where `lake build Tablet.<Node>` leaves every other node's
        # `.olean` alone rather than purging the ones it did not need.
        for theory in accepted:
            sha = self._theory_sha(master_dir, theory)
            if sha is None:
                # The projected file vanished between the caller's listing and
                # now; skip it (a later reconcile will retry once present).
                continue
            if self._promoted.get(theory) == sha:
                continue  # already warm with these exact bytes
            promote_timeout = timeout_secs
            if deadline is not None:
                remaining = deadline - time.monotonic()
                if remaining <= 0.0:
                    # PRE-DISPATCH exhaustion: no promote task is in flight
                    # (the socket is idle), so the session is safe to keep —
                    # signalled by the distinct type. A ``timed_out`` raised
                    # by the promote itself (below) stays the base class:
                    # the server may still be elaborating that task.
                    raise IsabelleReconcileBudgetExhausted(
                        "timed_out",
                        f"reconcile budget exhausted before promoting {theory} "
                        f"({float(total_budget_secs):.0f}s shared across the "
                        f"cone); already-promoted members remain warm",
                    )
                promote_timeout = remaining
            # Content changed (or first sight): re-`use_theories` the file
            # directly. We deliberately do NOT `evict` first. The public
            # `purge_theories` server command does not actually purge on
            # 2025-2 — `Resources.purge_theories` computes the document edits
            # and then DISCARDS them instead of applying them via
            # `session.update` (`src/Pure/PIDE/headless.scala`; the internal
            # `clean_theories` is the path that applies them). Purge-then-
            # reload under the same name is what produced the live-confirmed
            # "Illegal theory header". Re-loading changed files through
            # `use_theories` is the documented supported path (NEWS, Isabelle2022).
            self.promote(
                master_dir=master_dir, theory=theory, timeout_secs=promote_timeout
            )

    @staticmethod
    def _parse_use_theories_reply(
        reply: IsabelleReply,
        theory: str,
        cert_theorem: Optional[str],
    ) -> CheckOutcome:
        payload = reply.payload
        ok = bool(payload.get("ok", False))
        nodes = payload.get("nodes")
        # Locate our theory's node (single-theory request). The server
        # qualifies the name as ``Draft.<theory>`` when no session ROOT
        # owns it; match on the trailing component.
        # `None`, not `{}`. An absent node must stay DISTINGUISHABLE from a node
        # that reported nothing: `{}` is a `dict`, so the `pre_node_failure`
        # test below (`node is None or not isinstance(node, dict)`) could never
        # fire on the one path it exists for — the reply that carries no node at
        # all. That is why a transient `use_theories` failure kept being parsed
        # as "theorem absent, empty statement, empty axioms" and surfaced as a
        # MATHEMATICAL `invalid_proof`, halting two runs on a certificate
        # disagreement that never existed.
        node: Optional[Mapping[str, Any]] = None
        if isinstance(nodes, list):
            for nd in nodes:
                if not isinstance(nd, dict):
                    continue
                tn = str(nd.get("theory_name", ""))
                if tn == theory or tn.rsplit(".", 1)[-1] == theory:
                    node = nd
                    break
            # No `nodes[0]` fallback. When the requested theory is absent the
            # first returned node is a DIFFERENT theory (a dependency), and
            # building the certificate from it attributes another theory's
            # status to this node. Absence is a channel failure; report it.
        status = node.get("status", {}) if isinstance(node, dict) else {}
        failed = int(status.get("failed", 0) or 0) if isinstance(status, dict) else 0
        finished = (
            int(status.get("finished", 0) or 0) if isinstance(status, dict) else 0
        )
        # On a FAILED terminal reply (server-level) with no node ok flag,
        # treat it as failed even if the per-node count was absent.
        if reply.kind == "FAILED":
            ok = False
            if failed == 0:
                failed = 1

        # A reply that carries NO node for this theory is a PRE-CERTIFICATE
        # failure: `use_theories` gave up during import/dependency resolution and
        # never produced a snapshot. Isabelle reports that at the reply's TOP
        # LEVEL (`kind`/`message`, plus a top-level `errors` list), not under
        # `nodes[*].messages`.
        #
        # Reading diagnostics only from the node made such a reply indistinguishable
        # from a completed run of a broken proof: no theorem, no statement, no
        # dependencies, no errors — which the cert layer then labelled
        # `invalid_proof`, a MATHEMATICAL verdict. The warm/cold cross-check
        # compared a clean warm certificate against that fabricated refutation and
        # HALTED a live run on a closure "disagreement" that did not exist.
        #
        # Capture the top-level diagnostic so the cause survives, and mark the
        # outcome as a transport failure rather than a verdict.
        pre_node_failure = node is None
        top_level_errors: List[str] = []
        payload_obj = reply.payload if isinstance(reply.payload, dict) else {}
        top_message = str(payload_obj.get("message", "") or "").strip()
        if top_message:
            top_level_errors.append(top_message)
        raw_top_errors = payload_obj.get("errors")
        if isinstance(raw_top_errors, list):
            for item in raw_top_errors:
                if isinstance(item, dict):
                    text = str(item.get("message", "") or "").strip()
                else:
                    text = str(item or "").strip()
                if text:
                    top_level_errors.append(text)
        tail = str(getattr(reply, "raw_tail", "") or "").strip()
        if pre_node_failure and not top_level_errors and tail:
            top_level_errors.append(tail)

        writeln_lines: List[str] = []
        error_lines: List[str] = []
        warning_lines: List[str] = []
        error_lines.extend(top_level_errors)
        messages = node.get("messages", []) if isinstance(node, dict) else []
        if isinstance(messages, list):
            for mm in messages:
                if not isinstance(mm, dict):
                    continue
                kind = str(mm.get("kind", ""))
                text = str(mm.get("message", ""))
                if kind == "writeln":
                    writeln_lines.append(text)
                elif kind == "error":
                    error_lines.append(text)
                elif kind == "warning":
                    warning_lines.append(text)

        (
            oracles,
            dependencies,
            theorem_exists,
            extra_shyps,
            statement_repr,
            statement_repr_long,
            residual_oracles,
            boundary_theorems,
            statement_type_repr,
            statement_repr_typed,
        ) = _parse_cert_from_writeln(writeln_lines, cert_theorem)
        normalized_stmt = normalize_statement_repr(statement_repr)
        # The structural payload is already single-line printable ASCII, so
        # `normalize_statement_repr` is a near-no-op on it; run it anyway so
        # the type axis and the print axes share ONE canonicalizer and cannot
        # drift apart.
        normalized_type_repr = normalize_statement_repr(statement_type_repr)
        return CheckOutcome(
            ok=ok,
            failed=failed,
            finished=finished,
            theory_name=str(node.get("theory_name", theory)) if node else theory,
            node_name=str(node.get("node_name", "")) if node else "",
            writeln_lines=writeln_lines,
            error_lines=error_lines,
            warning_lines=warning_lines,
            oracles=oracles,
            dependencies=dependencies,
            theorem_exists=theorem_exists,
            extra_shyps=extra_shyps,
            pre_node_failure=pre_node_failure,
            residual_oracles=residual_oracles,
            boundary_theorems=boundary_theorems,
            statement_repr=normalized_stmt,
            statement_hash=statement_hash_of(statement_repr),
            statement_repr_long=normalize_statement_repr(statement_repr_long),
            statement_type_repr=normalized_type_repr,
            statement_type_hash=statement_hash_of(statement_type_repr),
            statement_repr_typed=normalize_statement_repr(statement_repr_typed),
        )

    # ------------------------------ wire I/O ------------------------------

    def _send(self, command: str, arg: Optional[Mapping[str, Any]] = None) -> None:
        if self._sock is None:
            raise IsabelleSessionError("protocol_error", "socket not connected")
        if arg is None:
            line = command
        else:
            line = f"{command} {json.dumps(arg, separators=(',', ':'))}"
        try:
            self._sock.sendall((line + "\n").encode("utf-8"))
        except OSError as exc:
            raise IsabelleSessionError(
                "protocol_error", f"failed to send {command}: {exc}"
            )

    def _await_terminal(self, *, timeout_secs: float) -> IsabelleReply:
        """Read async replies until a terminal kind (FINISHED/FAILED/ERROR).

        ``OK`` opens a task and ``NOTE`` is progress — both are consumed and
        skipped. Returns the first terminal reply. Raises ``timed_out`` if
        the budget elapses without one.
        """
        deadline = time.monotonic() + max(1.0, float(timeout_secs))
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise IsabelleSessionError(
                    "timed_out",
                    f"no terminal reply within {timeout_secs}s",
                )
            # Give each read the FULL remaining budget. The server streams
            # NOTE progress during a long build, so a recv rarely blocks
            # the whole budget; recomputing per-read keeps the overall
            # deadline honoured without starving a genuinely slow build of
            # wall-clock between progress messages.
            if self._sock is not None:
                self._sock.settimeout(remaining)
            reply = self._read_reply()
            if reply is None:
                raise IsabelleSessionError(
                    "protocol_error", "server closed the connection unexpectedly"
                )
            if reply.kind in _TERMINAL_KINDS:
                return reply
            # OK / NOTE → keep reading.

    def _read_reply(self) -> Optional[IsabelleReply]:
        body = self._read_message()
        if body is None:
            return None
        text = body.decode("utf-8", errors="replace")
        # ``<KIND> <JSON>``; KIND is the first whitespace-delimited token.
        head, _, tail = text.partition(" ")
        kind = head.strip()
        tail = tail.strip()
        if kind not in _REPLY_KINDS:
            # Defensive: an unframed/echo line. Treat as a NOTE-equivalent
            # progress line so the loop keeps going rather than crashing.
            return IsabelleReply(kind="NOTE", payload={}, raw_tail=text[:500])
        payload: Mapping[str, Any] = {}
        if tail:
            try:
                decoded = json.loads(tail)
            except json.JSONDecodeError:
                decoded = None
            if isinstance(decoded, dict):
                payload = decoded
        return IsabelleReply(kind=kind, payload=payload, raw_tail=tail[:2000])

    def _read_message(self) -> Optional[bytes]:
        """Read one framed message body. Handles both single-line and the
        ``<decimal-length>\\n<body>`` block framing.

        Returns the message body (without the framing) or ``None`` on EOF.
        """
        line = self._read_raw_line()
        if line is None:
            return None
        stripped = line.strip()
        if _LENGTH_PREFIX_RE.match(stripped):
            n = int(stripped)
            body = self._read_exact(n)
            if body is None:
                return None
            # The declared length INCLUDES the body's trailing newline (nothing
            # follows the n bytes on the wire); strip it so block and single-line
            # messages both yield the bare content.
            if body.endswith(b"\n"):
                body = body[:-1]
            return body
        return line

    def _read_raw_line(self) -> Optional[bytes]:
        while True:
            idx = self._buf.find(b"\n")
            if idx >= 0:
                line = bytes(self._buf[:idx])
                del self._buf[: idx + 1]
                return line
            chunk = self._recv_chunk()
            if chunk is None:
                if self._buf:
                    rest = bytes(self._buf)
                    self._buf.clear()
                    return rest
                return None
            self._buf.extend(chunk)

    def _read_exact(self, n: int) -> Optional[bytes]:
        while len(self._buf) < n:
            chunk = self._recv_chunk()
            if chunk is None:
                return None
            self._buf.extend(chunk)
        body = bytes(self._buf[:n])
        del self._buf[:n]
        return body

    def _recv_chunk(self) -> Optional[bytes]:
        if self._sock is None:
            return None
        try:
            chunk = self._sock.recv(_RECV_CHUNK)
        except socket.timeout:
            raise IsabelleSessionError(
                "timed_out", "timed out reading from isabelle server"
            )
        except OSError as exc:
            raise IsabelleSessionError(
                "protocol_error", f"recv failed: {exc}"
            )
        if not chunk:
            return None
        return chunk


# ----------------------- cert parsing (pure helpers) -----------------------


def _parse_cert_from_writeln(
    writeln_lines: Sequence[str],
    cert_theorem: Optional[str],
) -> Tuple[
    List[str], List[str], bool, List[str], str, str, List[str], List[str], str, str
]:
    """Parse the probe theory's ``writeln`` certificate output.

    Returns ``(oracles, dependencies, theorem_exists, extra_shyps,
    statement_repr, statement_repr_long, residual_oracles, boundary_theorems,
    statement_type_repr, statement_repr_typed)``. Each writeln "message" may
    itself be multi-line
    (e.g. the ``thm_deps`` block is a single message whose body is
    ``"dependencies: N\\n    name1\\n …"``), so we flatten to physical
    lines first — but we keep each line TAGGED with its originating message
    index, because the message boundary is what delimits a marker payload
    (see :func:`_collect_marker_payload`).

    Grammar (wire-verified + the B2c-gate Slice-1 probe lines):
      * ``theorem <name>: <prop>`` — emitted by both commands; presence
        with a matching name proves the theorem exists (catches ``oops``/
        elision, which emit no such line).
      * ``oracles:`` then 0+ indented oracle names (``skip_proof`` for a
        ``sorry`` proof; empty for a clean one).
      * ``dependencies: N`` then indented dep names.
      * ``TRELLIS_SHYPS <sort> …`` — the residual dangling sort hypotheses
        the probe's ``Thm.extra_shyps (Thm.strip_shyps thm)`` ML emits. A
        NON-EMPTY payload ⇒ the theorem rests on an empty/inconsistent
        type class (S3, oracle-blind). Empty payload ⇒ no residual shyps.
      * ``TRELLIS_STMT <prop>`` — the probe's ``YXML.content_of
        (Syntax.string_of_term … (Thm.prop_of thm))`` ML emits the
        elaborated statement here. Returned UN-normalized as
        ``statement_repr`` (the caller normalizes + hashes). Constants print
        SHORT (unqualified); this is the SOUNDNESS record's self-stability
        statement.
      * ``TRELLIS_STMT_LONG <prop>`` — the SAME ``Thm.prop_of`` printed with
        ``Name_Space.names_long`` so cross-node constants carry their defining
        ``Tablet_<Dep>.`` qualifier. Returned UN-normalized as
        ``statement_repr_long``; the Correspondence Tier-2 closure axis reads
        it (the qualified names map to sibling NodeIds). A worker NODE theory
        cannot emit it (the same S6 ``ML``-ban that protects ``TRELLIS_STMT``).
      * ``TRELLIS_STMT_STRUCT <payload>`` — the canonical STRUCTURAL
        serialization of the theorem (``ML_Syntax.print_term`` of the
        canonicalized ``Thm.full_prop_of``, tupled with ``Thm.hyps_of`` and
        ``Thm.extra_shyps``). Returned UN-normalized as
        ``statement_type_repr``; the caller normalizes + hashes it into
        ``statement_type_hash``. This is the axis that sees TYPES and SORTS.
      * ``TRELLIS_STMT_TYPED <prop>`` — the ``show_types``/``show_sorts``
        human-readable companion. DIAGNOSTIC ONLY (never hashed, never gated).

    The ``TRELLIS_*`` lines are CHECKER-AUTHORED probe output, never worker
    text — a worker's NODE theory cannot emit them (the S6 shape gate bans
    ``ML``/``thm_oracles`` in node theories, and the probe references the
    theorem by SERVER-constructed qualified name).
    """
    # Keep the originating message index alongside each physical line. A
    # ``writeln`` message is ONE marker (the probe emits one ``writeln`` per
    # marker), so the message boundary is the payload's outer delimiter; the
    # explicit ``TRELLIS_MARKER_EOM`` sentinel is the inner one. Both are
    # needed: batch-mode ``Pretty`` wraps at 76 columns
    # (``pretty.ML:264,418``, ``ml_pretty.ML:125``), so a marker payload is
    # NOT guaranteed to be one physical line — the pre-fix parser kept only
    # the first physical line and silently truncated the rest.
    lines: List[Tuple[str, int]] = []
    for msg_index, msg in enumerate(writeln_lines):
        for physical in msg.splitlines():
            lines.append((physical, msg_index))

    oracles: List[str] = []
    dependencies: List[str] = []
    theorem_exists = False
    extra_shyps: List[str] = []
    statement_repr = ""
    statement_repr_long = ""
    statement_type_repr = ""
    statement_repr_typed = ""

    residual_oracles: List[str] = []
    boundary_theorems: List[str] = []
    section: Optional[str] = None  # "oracles" | "dependencies" | None
    index = 0
    while index < len(lines):
        raw, _msg_index = lines[index]
        stripped = raw.strip()
        if not stripped:
            index += 1
            continue
        # The probe's ML marker lines end any active oracles/dependencies
        # section and own every following physical line of their own message
        # up to the sentinel (see `_collect_marker_payload`).
        tag = _marker_tag_of(stripped)
        if tag is not None:
            section = None
            payload, index = _collect_marker_payload(lines, index, tag)
            if tag == TRELLIS_RESIDUAL_TAG:
                if payload:
                    residual_oracles.extend(_split_names(payload))
            elif tag == TRELLIS_BOUNDARY_TAG:
                if payload:
                    boundary_theorems.extend(_split_names(payload))
            elif tag == TRELLIS_SHYPS_TAG:
                if payload:
                    extra_shyps.extend(_split_names(payload))
            elif tag == TRELLIS_STMT_STRUCT_TAG:
                if payload and not statement_type_repr:
                    statement_type_repr = payload
            elif tag == TRELLIS_STMT_TYPED_TAG:
                if payload and not statement_repr_typed:
                    statement_repr_typed = payload
            elif tag == TRELLIS_STMT_LONG_TAG:
                if payload and not statement_repr_long:
                    statement_repr_long = payload
            elif tag == TRELLIS_STMT_TAG:
                # First non-empty STMT wins (the probe emits exactly one).
                if payload and not statement_repr:
                    statement_repr = payload
            continue
        index += 1
        # A header resets the active section.
        if stripped == "oracles:" or stripped.startswith("oracles:"):
            section = "oracles"
            # Some builds emit names on the same line after the colon.
            inline = stripped[len("oracles:"):].strip()
            if inline:
                oracles.extend(_split_names(inline))
            continue
        if stripped.startswith("dependencies:"):
            section = "dependencies"
            inline = stripped[len("dependencies:"):].strip()
            # The header is ``dependencies: <count>``; drop the leading int.
            inline = re.sub(r"^\d+\b", "", inline).strip()
            if inline:
                dependencies.extend(_split_names(inline))
            continue
        if stripped.startswith("theorem ") or stripped.startswith("theorems "):
            theorem_exists = True
            section = None
            continue
        # Indented continuation lines belong to the active section. The
        # writeln body indents names; a non-indented, non-header line ends
        # the section.
        is_indented = raw[:1] in (" ", "\t")
        if section == "oracles" and is_indented:
            oracles.extend(_split_names(stripped))
        elif section == "dependencies" and is_indented:
            dependencies.extend(_split_names(stripped))
        else:
            section = None

    # If the caller named a specific theorem, only trust ``theorem_exists``
    # when the matching name appeared (defends against a stray theorem line
    # from an unrelated lemma in the same theory). When unnamed, any
    # theorem line suffices.
    if cert_theorem:
        theorem_exists = any(
            _writeln_declares_theorem(msg, cert_theorem) for msg in writeln_lines
        )

    # De-dup while preserving order (the kernel stores these as sorted
    # sets, but a stable list keeps the observation deterministic).
    return (
        _dedup(oracles),
        _dedup(dependencies),
        theorem_exists,
        _dedup(extra_shyps),
        statement_repr,
        statement_repr_long,
        _dedup(residual_oracles),
        _dedup(boundary_theorems),
        statement_type_repr,
        statement_repr_typed,
    )


def _marker_tag_of(stripped: str) -> Optional[str]:
    """The ``TRELLIS_*`` marker tag this line opens, if any.

    Matched LONGEST-FIRST so a tag that is a textual prefix of another
    (``TRELLIS_STMT`` prefixes ``TRELLIS_STMT_LONG``, ``TRELLIS_STMT_STRUCT``
    and ``TRELLIS_STMT_TYPED``) can never shadow it. Deriving the order from
    ``len`` rather than from hand-written ``if`` ordering makes the hazard
    structurally impossible to reintroduce when the next marker is added.
    """
    for tag in _TRELLIS_TAGS_LONGEST_FIRST:
        if stripped.startswith(tag):
            return tag
    return None


def _is_cert_section_header(stripped: str) -> bool:
    """Is this a ``thm_oracles``/``thm_deps`` structural line (not payload)?"""
    return (
        stripped.startswith("oracles:")
        or stripped.startswith("dependencies:")
        or stripped.startswith("theorem ")
        or stripped.startswith("theorems ")
    )


def _collect_marker_payload(
    lines: Sequence[Tuple[str, int]], index: int, tag: str
) -> Tuple[str, int]:
    """Collect one ``TRELLIS_*`` marker payload; return ``(payload, next)``.

    The payload is NOT assumed to be one physical line. Isabelle's batch-mode
    ``Pretty`` output ops wrap at the 76-column default margin
    (``pretty.ML:264,418-481``; ``ml_pretty.ML:125`` for the default), so a
    long ``Syntax.string_of_term`` print arrives as several physical lines
    inside a single ``writeln`` message. The pre-fix parser matched the tag as
    a LINE prefix, took the rest of that line, and dropped every continuation
    line — silently fingerprinting a PREFIX of the statement. That it never
    bit is a side condition of the PIDE print mode (``symbolic_output_ops``
    renders breaks as spaces, ``pretty.ML:271,505``), not a property of the
    parser.

    Two independent delimiters, both honoured:

    * the **explicit sentinel** ``TRELLIS_MARKER_EOM``, appended by the probe
      after the payload — the payload is cut there;
    * the **message boundary** — the probe emits exactly one ``writeln`` per
      marker, so a line from a different message never continues this
      payload. This is what keeps pre-sentinel output (recorded traces, older
      probes) parsing exactly as before.

    Belt and braces, a sibling ``TRELLIS_*`` tag or a ``thm_oracles`` /
    ``thm_deps`` structural line also terminates the payload, so a transport
    that ever merged messages could not make one marker swallow another.
    """
    msg_index = lines[index][1]
    chunks: List[str] = [lines[index][0].strip()[len(tag):].strip()]
    next_index = index + 1
    while TRELLIS_MARKER_EOM not in chunks[-1]:
        if next_index >= len(lines):
            break
        text, msg = lines[next_index]
        if msg != msg_index:
            break
        stripped = text.strip()
        if _marker_tag_of(stripped) is not None or _is_cert_section_header(stripped):
            break
        chunks.append(stripped)
        next_index += 1
    payload = " ".join(chunk for chunk in chunks if chunk)
    cut = payload.find(TRELLIS_MARKER_EOM)
    if cut >= 0:
        payload = payload[:cut]
    return payload.strip(), next_index


# Common Isabelle math/logic symbols in BOTH spellings: the Unicode glyph
# and the ``\<name>`` ASCII symbol spelling denote the SAME logical token.
# ``Syntax.string_of_term`` may render either depending on the term/print
# mode; we canonicalize Unicode → the symbol spelling so the statement hash
# is spelling-stable (R1: Correspondence I1 reuses this normalized string).
# Not exhaustive — it covers the connectives a HOL statement surfaces; any
# unmapped glyph passes through unchanged (still deterministic per install).
_ISABELLE_SYMBOL_CANON = {
    "∀": r"\<forall>",       # ∀
    "∃": r"\<exists>",       # ∃
    "∄": r"\<nexists>",      # ∄
    "∧": r"\<and>",          # ∧
    "∨": r"\<or>",           # ∨
    "¬": r"\<not>",          # ¬
    "⟹": r"\<Longrightarrow>",  # ⟹
    "⟸": r"\<Longleftarrow>",   # ⟸
    "⟺": r"\<Longleftrightarrow>",  # ⟺
    "⟶": r"\<longrightarrow>",  # ⟶
    "⟵": r"\<longleftarrow>",   # ⟵
    "⟷": r"\<longleftrightarrow>",  # ⟷
    "→": r"\<rightarrow>",   # →
    "←": r"\<leftarrow>",    # ←
    "⇒": r"\<Rightarrow>",   # ⇒
    "λ": r"\<lambda>",       # λ
    "≡": r"\<equiv>",        # ≡
    "≠": r"\<noteq>",        # ≠
    "≤": r"\<le>",           # ≤
    "≥": r"\<ge>",           # ≥
    "∈": r"\<in>",           # ∈
    "∉": r"\<notin>",        # ∉
    "⊆": r"\<subseteq>",     # ⊆
    "⊂": r"\<subset>",       # ⊂
    "∪": r"\<union>",        # ∪
    "∩": r"\<inter>",        # ∩
    "∅": r"\<emptyset>",     # ∅
    "×": r"\<times>",        # ×
    "⦃": r"\<lbrace>",       # ⦃
    "⦄": r"\<rbrace>",       # ⦄
}

# Isabelle cartouche delimiters (term-quotation markers) — drop them so a
# quoted vs unquoted rendering hashes identically.
_CARTOUCHE_DELIMS = {
    "‹": "",   # ‹  (open cartouche glyph)
    "›": "",   # ›  (close cartouche glyph)
    r"\<open>": "",
    r"\<close>": "",
}

# YXML control chars (X = \x05, Y = \x06). ``YXML.content_of`` strips markup
# inside ML, but we defensively re-strip any that leak onto the wire.
_YXML_X = "\x05"
_YXML_Y = "\x06"
_YXML_MARKUP_RE = re.compile(
    "\x05\x06[^\x05]*\x05\x06|[\x05\x06]"
)
_WS_RE = re.compile(r"\s+")


def normalize_statement_repr(text: str) -> str:
    """Canonicalize a ``TRELLIS_STMT`` payload to a spelling-stable form.

    Pipeline (R1): defensively strip any leaked YXML markup → drop cartouche
    delimiters → Unicode-NFC → canonicalize Unicode logic/math glyphs to
    their ``\\<name>`` symbol spelling → collapse all whitespace runs to a
    single space → strip. Deterministic; idempotent. The result is what the
    statement hash digests and what Correspondence I1 reuses.
    """
    import unicodedata

    if not text:
        return ""
    # 1. Strip leaked YXML markup control chars (defensive — the ML already
    #    applies YXML.content_of).
    out = _YXML_MARKUP_RE.sub("", text)
    # 2. Drop cartouche delimiters (both glyph + symbol spellings).
    for src, dst in _CARTOUCHE_DELIMS.items():
        if src:
            out = out.replace(src, dst)
    # 3. Unicode NFC (compose any decomposed sequences).
    out = unicodedata.normalize("NFC", out)
    # 4. Unicode glyph → symbol spelling (so ∀ and \<forall> converge).
    out = "".join(_ISABELLE_SYMBOL_CANON.get(ch, ch) for ch in out)
    # 5. Whitespace-collapse + strip.
    out = _WS_RE.sub(" ", out).strip()
    return out


def statement_hash_of(statement_repr: str) -> str:
    """``sha256`` of the NORMALIZED statement repr (empty repr → empty hash).

    Returning ``""`` for an empty/absent statement keeps the cert fail-closed:
    a missing ``TRELLIS_STMT`` yields no hash, so the gate cannot mistake it
    for a stable statement.
    """
    normalized = normalize_statement_repr(statement_repr)
    if not normalized:
        return ""
    return hashlib.sha256(normalized.encode("utf-8")).hexdigest()


def _writeln_declares_theorem(message: str, name: str) -> bool:
    for line in message.splitlines():
        s = line.strip()
        if s.startswith("theorem ") and (
            s[len("theorem "):].startswith(f"{name}:")
            or s[len("theorem "):].startswith(f"{name} ")
        ):
            return True
    return False


def _split_names(text: str) -> List[str]:
    return [tok for tok in re.split(r"[\s,]+", text.strip()) if tok]


def _dedup(items: Sequence[str]) -> List[str]:
    seen: set[str] = set()
    out: List[str] = []
    for it in items:
        if it not in seen:
            seen.add(it)
            out.append(it)
    return out


# ----------------------- theory authoring (pure helper) -----------------------


def cert_probe_theory_name(node_theory: str, *, nonce: Optional[str] = None) -> str:
    """The CHECKER-OWNED probe theory name for a node theory.

    ``Tablet_<Node>`` → ``Tablet_<Node>__Cert`` (or
    ``Tablet_<Node>__Cert_<nonce>`` when a ``nonce`` is given). The ``__Cert``
    infix is reserved for the checker; the S6 shape gate forbids a worker NODE
    theory from carrying it, so the probe never collides with worker-authored
    theories, and the scaffold rejects any ``*__Cert*`` stem from node
    enumeration (H3).

    A per-call ``nonce`` gives each cert probe a UNIQUE theory name on a
    held-open WARM session. PIDE retains stale document state for a REUSED
    theory name across ``use_theories`` calls — so re-probing the same node a
    second time on a warm session with the fixed name fails ("Illegal theory
    header" / the probe fails to re-elaborate; live-confirmed). A fresh name per
    probe sidesteps that entirely (the cold one-shot path, which discards the
    session, may pass ``nonce=None`` for the stable name).
    """
    base = f"{node_theory}__Cert"
    return base if nonce is None else f"{base}_{nonce}"


# The writeln tags the cut-walk emits; parsed alongside `thm_oracles`/`thm_deps`.
TRELLIS_RESIDUAL_TAG = "TRELLIS_RESIDUAL_ORACLES"
TRELLIS_BOUNDARY_TAG = "TRELLIS_BOUNDARY"
# The B2c-gate Slice-1 / corr statement markers.
TRELLIS_SHYPS_TAG = "TRELLIS_SHYPS"
TRELLIS_STMT_TAG = "TRELLIS_STMT"
TRELLIS_STMT_LONG_TAG = "TRELLIS_STMT_LONG"
TRELLIS_STMT_STRUCT_TAG = "TRELLIS_STMT_STRUCT"
TRELLIS_STMT_TYPED_TAG = "TRELLIS_STMT_TYPED"

# Explicit end-of-marker sentinel appended by the probe after every marker
# payload. Plain ASCII with no `<`/`>`/`&`, so `XML.content_of (YXML.parse_body
# …)` round-trips it unchanged, and no Isabelle symbol or term print can
# produce it. Stripped before normalization, so a payload's hash is
# byte-identical to the pre-sentinel one.
TRELLIS_MARKER_EOM = "@@TRELLIS_EOM@@"

_TRELLIS_TAGS: Tuple[str, ...] = (
    TRELLIS_RESIDUAL_TAG,
    TRELLIS_BOUNDARY_TAG,
    TRELLIS_SHYPS_TAG,
    TRELLIS_STMT_TAG,
    TRELLIS_STMT_LONG_TAG,
    TRELLIS_STMT_STRUCT_TAG,
    TRELLIS_STMT_TYPED_TAG,
)
# Longest-first: `TRELLIS_STMT` is a prefix of three siblings, so prefix order
# — not declaration order — decides which tag a line opens.
_TRELLIS_TAGS_LONGEST_FIRST: Tuple[str, ...] = tuple(
    sorted(_TRELLIS_TAGS, key=len, reverse=True)
)


def _ml_string_list(names: "Sequence[str]") -> str:
    """Render `names` as an SML string list literal."""
    return "[" + ", ".join('"' + n.replace("\\", "\\\\").replace('"', '\\"') + '"'
                           for n in names) + "]"


def _structural_statement_ml(qualified_thm: str) -> str:
    """CHECKER-OWNED ``ML`` emitting ``TRELLIS_STMT_STRUCT`` — the TYPE axis.

    Why a structural digest and not a better print. ``Syntax.string_of_term``
    is ``Pretty.string_of oo pretty_term`` and ``pretty_term`` is
    ``uncheck_terms #> unparse_term`` (``syntax.ML:335,341``); the *uncheck*
    phase's documented job is to "prune type-information before pretty
    printing" (``Doc/Implementation/Syntax.thy:198``), and the manual states
    outright that "the default configuration routinely looses information"
    (``:65``). Constants are never type-annotated in plain text at all —
    ``show_const_types = show_markup andalso show_consts_markup``
    (``syntax_phases.ML:665``) and this probe strips markup via
    ``XML.content_of (YXML.parse_body …)``. That is exactly how a worker
    generalized ``fixes R :: "nat \\<Rightarrow> nat \\<Rightarrow> bool"`` to
    ``"'a \\<Rightarrow> 'a \\<Rightarrow> bool"`` with a byte-identical print
    and an unmoved fingerprint.

    ``Term.term`` has no such hole: every ``Const``/``Free``/``Var``/``Abs``
    leaf carries its full ``typ`` and every ``TFree``/``TVar`` its ``sort``
    (``term.ML:217-234``), and ``ML_Syntax.print_term`` (``ml_syntax.ML:124``)
    is a total, theory-, config-, print-mode- and name-space-independent
    rendering of that datatype whose ``print_string`` escapes ``\\n``, ``\\t``,
    every control character and every byte >= 127 (``:66-83``) — so the
    payload is single-line printable ASCII by construction.

    What this digest decides is *precisely* Isabelle's own notion of "the same
    theorem statement": ``Thm.eq_thm_prop = op aconv o apply2
    Thm.full_prop_of`` (``more_thm.ML:216``) extended over all four components
    of ``Thm.thm_ord`` (``thm.ML:568-575``) — ``prop``, ``tpairs``, ``hyps``,
    ``shyps``:

    * ``Thm.full_prop_of`` (``thm.ML:69``) rather than ``Thm.prop_of``: it is
      ``attach_tpairs tpairs prop``, so flex-flex pairs (component 2) are in.
    * ``Thm.hyps_of`` (``:77``, component 3) and ``Thm.extra_shyps``
      (``:124``, component 4) are tupled into the payload. Both are
      unfingerprinted today, and a fact carrying local hypotheses is a
      strictly WEAKER theorem than the same fact without them.
    * ``Envir.beta_eta_contract`` (``envir.ML:300``) so eta-noise is not
      meaning (today's print is eta-stable; we must not regress that).
    * ``Term_Subst.zero_var_indexes_list`` (``term_subst.ML:30``) so ``?a1``
      vs ``?a2`` index churn is invisible. The LIST form, not the singleton
      ``zero_var_indexes``: hyps and prop must be renumbered JOINTLY, or a
      hypothesis's schematic variable would be renumbered independently of
      the prop's and two different (hyps, prop) pairings could collide.
    * ``Term.map_abs_vars (K "")`` (``term.ML:175``) erases ``Abs`` binder
      name hints, which are printing hints only (bound variables are de
      Bruijn, ``term.ML:233``) — this is what makes the digest decide
      ``aconv`` exactly rather than approximate it.

    Deliberately NOT rename-canonical: renaming ``fixes R`` to ``fixes S``
    still moves the digest, exactly as today's print does. Sledgehammer's
    ``normalize_vars`` (``sledgehammer_fact.ML:105-125``) is the in-tree
    upgrade path if rename churn ever becomes review load; it cannot mask a
    meaning change, since renaming is meaning-preserving.
    """
    eom = TRELLIS_MARKER_EOM
    return (
        "ML ‹\n"
        "  let\n"
        f"    val thm = Thm.strip_shyps @{{thm {qualified_thm}}};\n"
        "    val terms = Thm.full_prop_of thm :: Thm.hyps_of thm;\n"
        "    val canon =\n"
        "      map (Term.map_abs_vars (K \"\"))\n"
        "        (Term_Subst.zero_var_indexes_list "
        "(map Envir.beta_eta_contract terms));\n"
        "    val body =\n"
        "      ML_Syntax.print_list ML_Syntax.print_sort "
        "(Thm.extra_shyps thm) ^ \"|\" ^\n"
        "      ML_Syntax.print_list ML_Syntax.print_term (tl canon) ^ \"|\" ^\n"
        "      ML_Syntax.print_term (hd canon);\n"
        "  in\n"
        f"    writeln (\"{TRELLIS_STMT_STRUCT_TAG} \" ^ body ^ \" {eom}\")\n"
        "  end›\n"
    )


def _typed_statement_print_ml(qualified_thm: str) -> str:
    """CHECKER-OWNED ``ML`` emitting ``TRELLIS_STMT_TYPED`` — the COMPANION.

    A ``show_types``/``show_sorts`` print of the same ``Thm.prop_of``.
    DIAGNOSTIC ONLY: it is never hashed and never gated, because it is not
    sound as a gate — constants stay unannotated without ``show_markup``
    (``syntax_phases.ML:665``, and the markup this probe strips), and
    ``prune_types`` (``:609-628``) annotates only the FIRST occurrence of each
    ``aconv``-distinct ``Free``/``Var``. Its job is purely that the next
    type-drift incident is self-explanatory on screen: the reviewer reads
    ``R :: nat \\<Rightarrow> nat \\<Rightarrow> bool`` against
    ``R :: 'a \\<Rightarrow> 'a \\<Rightarrow> bool`` instead of two identical
    prints and an unexplained hash move.

    ``show_abbrevs`` is pinned OFF so a widened session preamble introducing a
    new ``abbreviation`` cannot silently rewrite the print of an unchanged
    theorem (``proof_context.ML:726-734``), and ``names_long`` ON to match
    ``TRELLIS_STMT_LONG``. ``Pretty.unformatted_string_of`` (``pretty.ML:86``)
    rather than ``Syntax.string_of_term``: it renders breaks as spaces with no
    margin logic, so the print does not depend on batch-vs-PIDE output ops.
    """
    eom = TRELLIS_MARKER_EOM
    return (
        f"ML ‹writeln (\"{TRELLIS_STMT_TYPED_TAG} \" ^ "
        "XML.content_of (YXML.parse_body "
        "(Pretty.unformatted_string_of (Syntax.pretty_term "
        f"(@{{context}}\n"
        "   |> Config.put Printer.show_types true\n"
        "   |> Config.put Printer.show_sorts true\n"
        "   |> Config.put Printer.show_markup false\n"
        "   |> Config.put Proof_Context.show_abbrevs false\n"
        "   |> Config.put Name_Space.names_long true) "
        f"(Thm.prop_of @{{thm {qualified_thm}}})))) ^ \" {eom}\")›\n\n"
    )


def _cut_walk_ml(qualified_thm: str, boundary_thms: "Sequence[str]") -> str:
    """CHECKER-OWNED cut traversal of the principal's proof body.

    Attributes each oracle occurrence to the proof that INTRODUCED it, so a node
    whose only unproven dependency is a declared-open child is distinguishable
    from a node whose own proof is unfinished. `Thm_Deps.all_oracles` cannot make
    that distinction: it unions the whole transitive graph, so every ancestor of
    an open node reports `skip_proof` and would be rejected.

    Subtracting the child's oracle NAMES from the parent's is NOT a substitute and
    is unsound: two independent `skip_proof` occurrences are indistinguishable by
    name, so `{skip_proof} - {skip_proof} = {}` would silently accept a parent
    that also has its own `sorry`. Ownership must come from the proof structure.

    Traversal (mirrors the Lean local-closure probe, which cuts at the principal
    theorem of each declared Tablet dependency):

    * a `PBody`'s DIRECT `oracles` belong to the proof being visited → residual;
    * a referenced theorem node whose identity is a declared boundary is RECORDED
      and NOT entered — its own closure is that node's certificate to prove;
    * anything else (anonymous boxes, the root's own self-box, library lemmas) is
      descended into, so nothing is silently skipped.

    Identity is `Thm_Name.short`, which is ALREADY theory-qualified
    (`Tablet_Child.child`). `Proofterm.thm_node_theory_name` is SESSION-qualified
    (`Tablet.Tablet_Child`), so concatenating the two double-qualifies and matches
    nothing — verified experimentally. Anonymous boxes carry `""` and are never
    boundaries.

    `boundary_thms` is computed by the CHECKER from the node's declared imports;
    the worker never supplies it. `Proofterm.fold_body_thms` is deliberately not
    used: it descends into child bodies before invoking its callback, so it cannot
    express a cut.
    """
    bounds = _ml_string_list(boundary_thms)
    root = qualified_thm.replace("\\", "\\\\").replace('"', '\\"')
    return (
        "ML \\<open>\n"
        "  let\n"
        "    val boundaries = " + bounds + ";\n"
        '    val root = "' + root + '";\n'
        "    fun node_id node = Thm_Name.short (Proofterm.thm_node_name node);\n"
        "    val seen = Unsynchronized.ref Inttab.empty;\n"
        "    val residual = Unsynchronized.ref ([]: string list);\n"
        "    val hit = Unsynchronized.ref ([]: string list);\n"
        "    fun walk_body (Proofterm.PBody {oracles, thms, ...}) =\n"
        "      (residual := map (fn ((nm, _), _) => nm) oracles @ ! residual;\n"
        "       List.app walk_thm thms)\n"
        "    and walk_thm (i, node) =\n"
        "      let val id = node_id node in\n"
        '        if id <> "" andalso id <> root andalso member (op =) boundaries id\n'
        "        then hit := id :: ! hit\n"
        "        else if Inttab.defined (! seen) i then ()\n"
        "        else (seen := Inttab.update (i, ()) (! seen);\n"
        "              walk_body (Future.join (Proofterm.thm_node_body node)))\n"
        "      end;\n"
        "    val _ = walk_body (Thm.proof_body_of @{thm " + qualified_thm + "});\n"
        "  in\n"
        '    writeln ("' + TRELLIS_RESIDUAL_TAG + ' " ^ '
        'commas (sort_distinct string_ord (! residual)) ^ " '
        + TRELLIS_MARKER_EOM + '");\n'
        '    writeln ("' + TRELLIS_BOUNDARY_TAG + ' " ^ '
        'commas (sort_distinct string_ord (! hit)) ^ " '
        + TRELLIS_MARKER_EOM + '")\n'
        "  end\\<close>\n\n"
    )


def write_cert_probe_theory(
    master_dir: Path,
    node_theory: str,
    qualified_thm: str,
    *,
    probe_theory: Optional[str] = None,
    boundary_thms: "Optional[Sequence[str]]" = None,
) -> Path:
    """Write the CHECKER-OWNED probe theory the worker cannot author (S1).

    ``probe_theory`` overrides the probe theory NAME (default
    ``cert_probe_theory_name(node_theory)``). A caller on a held-open WARM
    session passes a per-call unique name so PIDE never reuses stale document
    state for the probe (see :func:`cert_probe_theory_name`); the file stem is
    set to match. The probe BODY (imports + cert commands) is name-independent.

    The probe is the SOLE source of the soundness certificate. It
    ``imports`` the already-checked worker node theory ``<node_theory>`` and
    references the principal theorem by its SERVER-constructed fully-
    qualified name ``<qualified_thm>`` (e.g. ``Tablet_<Node>.<node>``), then
    emits:

      * ``thm_oracles <qualified_thm>`` → the kernel-authentic oracle set
        (``skip_proof`` for ``sorry``; ``z3``/``smt`` for a solver — which
        the BUILD does not catch). [S1/S2]
      * ``thm_deps <qualified_thm>`` → the transitive theorem/axiom deps
        (the ``#print axioms`` analogue).
      * ``TRELLIS_SHYPS`` ← ``Thm.extra_shyps (Thm.strip_shyps thm)`` — the
        residual dangling sort hypotheses (empty/inconsistent-class
        vacuous-``False`` channel, oracle-blind). [S3]
      * ``TRELLIS_STMT`` ← ``YXML.content_of (Syntax.string_of_term …
        (Thm.prop_of thm))`` — the elaborated statement, constants SHORT
        (the soundness record's self-stability statement). [hardened gate #2]
      * ``TRELLIS_STMT_LONG`` ← the SAME ``Thm.prop_of`` printed under
        ``Name_Space.names_long`` so cross-node constants carry their defining
        ``Tablet_<Dep>.`` qualifier — the Correspondence Tier-2 closure axis
        (I2) reads it. Additive; the SHORT ``TRELLIS_STMT`` is byte-unchanged.
      * ``TRELLIS_STMT_STRUCT`` ← the STRUCTURAL digest input. See the block
        comment on its ``ML`` below; this is the only marker that carries the
        theorem's TYPES and SORTS, because every ``Syntax.string_of_term``
        print above deliberately prunes them.
      * ``TRELLIS_STMT_TYPED`` ← the ``show_types``/``show_sorts`` companion
        print. DIAGNOSTIC ONLY — never hashed, never gated.

    Every ``TRELLIS_*`` payload is terminated by the explicit
    ``TRELLIS_MARKER_EOM`` sentinel so a payload that Isabelle's batch-mode
    ``Pretty`` wraps at the 76-column margin is reassembled rather than
    truncated at its first physical line (see :func:`_collect_marker_payload`).

    Because the certificate is read from the kernel ``thm`` value resolved
    by qualified name in a CHECKER-authored file, the worker can neither
    forge it, omit it, nor redirect it at a trivial shadow theorem.

    ``ML ‹…›`` is forbidden in worker NODE theories (the S6 shape gate) but
    this probe is checker text, not worker input. Returns the written path;
    the session then drives ``use_theories`` against this probe theory.
    """
    master_dir.mkdir(parents=True, exist_ok=True)
    if probe_theory is None:
        probe_theory = cert_probe_theory_name(node_theory)
    # YXML-strip of the pretty-printed statement: ``Syntax.string_of_term``
    # may carry YXML markup; the canonical 2025-2 plain-text extraction is
    # ``XML.content_of (YXML.parse_body s)`` (the idiom in Pure's
    # ml_compiler.ML / protocol_message.ML). NOTE: ``YXML.content_of`` does
    # NOT exist in 2025-2 (live-confirmed) — using it errors the ML and
    # suppresses ``TRELLIS_STMT``. ``parse_body`` round-trips cleanly whether
    # or not the string actually carries markup.
    eom = TRELLIS_MARKER_EOM
    text = (
        f"theory {probe_theory}\n"
        f"  imports {node_theory}\n"
        f"begin\n\n"
        f"thm_oracles {qualified_thm}\n"
        f"thm_deps {qualified_thm}\n"
        f"ML ‹writeln (\"TRELLIS_SHYPS \" ^ commas (map "
        f"(Syntax.string_of_sort @{{context}}) "
        f"(Thm.extra_shyps (Thm.strip_shyps @{{thm {qualified_thm}}}))) "
        f"^ \" {eom}\")›\n"
        f"ML ‹writeln (\"TRELLIS_STMT \" ^ XML.content_of (YXML.parse_body "
        f"(Syntax.string_of_term @{{context}} "
        f"(Thm.prop_of @{{thm {qualified_thm}}}))) ^ \" {eom}\")›\n"
        # The Correspondence Tier-2 closure axis (I2) needs cross-node constants
        # printed WITH their defining ``Tablet_<Dep>.`` theory qualifier; the
        # default print is SHORT (a read-only probe confirmed it hides every
        # cross-node dependency). ``Config.put Name_Space.names_long true``
        # forces the fully-qualified form. This is a SEPARATE writeln; the SHORT
        # ``TRELLIS_STMT`` above (the soundness self-stability statement) is
        # byte-unchanged.
        f"ML ‹writeln (\"TRELLIS_STMT_LONG \" ^ XML.content_of (YXML.parse_body "
        f"(Syntax.string_of_term (Config.put Name_Space.names_long true @{{context}}) "
        f"(Thm.prop_of @{{thm {qualified_thm}}}))) ^ \" {eom}\")›\n\n"
        f"{_structural_statement_ml(qualified_thm)}"
        f"{_typed_statement_print_ml(qualified_thm)}"
        f"{_cut_walk_ml(qualified_thm, boundary_thms or [])}"
        f"end\n"
    )
    path = master_dir / f"{probe_theory}.thy"
    path.write_text(text, encoding="utf-8")
    return path


def write_theory_with_cert(
    *,
    master_dir: Path,
    theory: str,
    body: str,
    cert_theorem: str,
    imports: str = "Main",
) -> Path:
    """Write ``<master_dir>/<theory>.thy`` carrying the proof + cert commands.

    The S1 ANTI-PATTERN (now TEST-ONLY): co-locating ``thm_oracles`` /
    ``thm_deps`` for ``cert_theorem`` in the SAME ``.thy`` as the proof
    lets a worker forge/omit/redirect its own certificate. Production reads
    the certificate from a CHECKER-OWNED probe theory instead
    (:func:`write_cert_probe_theory`); this helper is retained only for the
    test harness and the opt-in live single-file smoke.

    ``body`` is the lemma/theorem block (e.g.
    ``lemma triv: "(1::nat)+1=2" by simp``). We append ``thm_oracles`` +
    ``thm_deps`` for ``cert_theorem`` so a single ``use_theories`` both
    checks the proof AND emits the soundness certificate via ``writeln``
    (Appendix A.4 — the version-stable, no-ML-coupling route).

    Returns the written path. The file is the ONLY thing written; the
    session object then drives ``use_theories`` against it.
    """
    master_dir.mkdir(parents=True, exist_ok=True)
    text = (
        f"theory {theory}\n"
        f"  imports {imports}\n"
        f"begin\n\n"
        f"{body.rstrip()}\n\n"
        f"thm_oracles {cert_theorem}\n"
        f"thm_deps {cert_theorem}\n\n"
        f"end\n"
    )
    path = master_dir / f"{theory}.thy"
    path.write_text(text, encoding="utf-8")
    return path


__all__ = [
    "DEFAULT_ISABELLE_BIN",
    "ISABELLE_BIN_ENV",
    "MIN_ISABELLE_VERSION",
    "isabelle_bin",
    "parse_isabelle_version",
    "servers_db_path",
    "IsabelleSession",
    "IsabelleSessionError",
    "IsabelleReply",
    "CheckOutcome",
    "WarmCheckOutcome",
    "cert_probe_theory_name",
    "write_cert_probe_theory",
    "write_theory_with_cert",
    "normalize_statement_repr",
    "statement_hash_of",
]
