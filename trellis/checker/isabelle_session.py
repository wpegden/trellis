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

Protocol (wire-verified — ``isabelle_install_notes.md`` Appendix A, and
a fresh end-to-end probe 2026-06-19)
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
* ``session_start {"session":"HOL"}`` → ``OK {task}`` → ``NOTE``… →
  ``FINISHED {session_id, tmp_dir, task}`` (HOL heap mmap'd warm).
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

# Absolute path to the Isabelle launcher (pinned; the kernel
# ``CheckerDriver::IsabelleHol`` op strings carry no path — the checker
# owns the binary location). Overridable via env for an alternate install.
DEFAULT_ISABELLE_BIN = "${TRELLIS_ROOT:-/path/to/trellis}/Isabelle2025-2/bin/isabelle"
ISABELLE_BIN_ENV = "TRELLIS_ISABELLE_BIN"

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
        session: str = "HOL",
        start_timeout_secs: float = DEFAULT_SESSION_START_TIMEOUT_SECS,
    ) -> None:
        if not name or not re.fullmatch(r"[A-Za-z0-9_.\-]+", name):
            raise IsabelleSessionError(
                "spawn_failed",
                f"invalid Isabelle server name {name!r} (must match [A-Za-z0-9_.-]+)",
            )
        self.name = name
        self.session = session
        self.start_timeout_secs = float(start_timeout_secs)
        self._bin = isabelle_bin_path or isabelle_bin()
        self._db_path = servers_db_path()
        self._proc: Optional[subprocess.Popen[str]] = None
        self._sock: Optional[socket.socket] = None
        self._buf = bytearray()
        self._session_id: Optional[str] = None
        self._port: Optional[int] = None

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
        self._send("session_start", {"session": self.session})
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
        """
        if self._sock is None or self._session_id is None:
            raise IsabelleSessionError(
                "protocol_error", "check_theory called before start()"
            )
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
        node: Mapping[str, Any] = {}
        if isinstance(nodes, list):
            for nd in nodes:
                if not isinstance(nd, dict):
                    continue
                tn = str(nd.get("theory_name", ""))
                if tn == theory or tn.rsplit(".", 1)[-1] == theory:
                    node = nd
                    break
            if not node and nodes and isinstance(nodes[0], dict):
                node = nodes[0]
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

        writeln_lines: List[str] = []
        error_lines: List[str] = []
        warning_lines: List[str] = []
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
        ) = _parse_cert_from_writeln(writeln_lines, cert_theorem)
        normalized_stmt = normalize_statement_repr(statement_repr)
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
            statement_repr=normalized_stmt,
            statement_hash=statement_hash_of(statement_repr),
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
) -> Tuple[List[str], List[str], bool, List[str], str]:
    """Parse the probe theory's ``writeln`` certificate output.

    Returns ``(oracles, dependencies, theorem_exists, extra_shyps,
    statement_repr)``. Each writeln "message" may itself be multi-line
    (e.g. the ``thm_deps`` block is a single message whose body is
    ``"dependencies: N\\n    name1\\n …"``), so we flatten to physical
    lines first.

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
        ``statement_repr`` (the caller normalizes + hashes).

    The ``TRELLIS_*`` lines are CHECKER-AUTHORED probe output, never worker
    text — a worker's NODE theory cannot emit them (the S6 shape gate bans
    ``ML``/``thm_oracles`` in node theories, and the probe references the
    theorem by SERVER-constructed qualified name).
    """
    lines: List[str] = []
    for msg in writeln_lines:
        lines.extend(msg.splitlines())

    oracles: List[str] = []
    dependencies: List[str] = []
    theorem_exists = False
    extra_shyps: List[str] = []
    statement_repr = ""

    section: Optional[str] = None  # "oracles" | "dependencies" | None
    for raw in lines:
        stripped = raw.strip()
        if not stripped:
            continue
        # The probe's ML marker lines are single-line and self-delimiting;
        # they end any active oracles/dependencies section.
        if stripped.startswith("TRELLIS_SHYPS"):
            section = None
            payload = stripped[len("TRELLIS_SHYPS"):].strip()
            if payload:
                extra_shyps.extend(_split_names(payload))
            continue
        if stripped.startswith("TRELLIS_STMT"):
            section = None
            payload = stripped[len("TRELLIS_STMT"):].strip()
            # First non-empty STMT wins (the probe emits exactly one).
            if payload and not statement_repr:
                statement_repr = payload
            continue
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
    )


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


def cert_probe_theory_name(node_theory: str) -> str:
    """The CHECKER-OWNED probe theory name for a node theory.

    ``Tablet_<Node>`` → ``Tablet_<Node>__Cert``. The double underscore +
    ``Cert`` suffix is reserved for the checker; the S6 shape gate forbids a
    worker NODE theory from being named this, so the probe never collides
    with worker-authored theories.
    """
    return f"{node_theory}__Cert"


def write_cert_probe_theory(
    master_dir: Path,
    node_theory: str,
    qualified_thm: str,
) -> Path:
    """Write the CHECKER-OWNED probe theory the worker cannot author (S1).

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
        (Thm.prop_of thm))`` — the elaborated statement (Correspondence I1
        reuses it). [hardened gate #2]

    Because the certificate is read from the kernel ``thm`` value resolved
    by qualified name in a CHECKER-authored file, the worker can neither
    forge it, omit it, nor redirect it at a trivial shadow theorem.

    ``ML ‹…›`` is forbidden in worker NODE theories (the S6 shape gate) but
    this probe is checker text, not worker input. Returns the written path;
    the session then drives ``use_theories`` against this probe theory.
    """
    master_dir.mkdir(parents=True, exist_ok=True)
    probe_theory = cert_probe_theory_name(node_theory)
    # YXML-strip of the pretty-printed statement: ``Syntax.string_of_term``
    # may carry YXML markup; the canonical 2025-2 plain-text extraction is
    # ``XML.content_of (YXML.parse_body s)`` (the idiom in Pure's
    # ml_compiler.ML / protocol_message.ML). NOTE: ``YXML.content_of`` does
    # NOT exist in 2025-2 (live-confirmed) — using it errors the ML and
    # suppresses ``TRELLIS_STMT``. ``parse_body`` round-trips cleanly whether
    # or not the string actually carries markup.
    text = (
        f"theory {probe_theory}\n"
        f"  imports {node_theory}\n"
        f"begin\n\n"
        f"thm_oracles {qualified_thm}\n"
        f"thm_deps {qualified_thm}\n"
        f"ML ‹writeln (\"TRELLIS_SHYPS \" ^ commas (map "
        f"(Syntax.string_of_sort @{{context}}) "
        f"(Thm.extra_shyps (Thm.strip_shyps @{{thm {qualified_thm}}}))))›\n"
        f"ML ‹writeln (\"TRELLIS_STMT \" ^ XML.content_of (YXML.parse_body "
        f"(Syntax.string_of_term @{{context}} "
        f"(Thm.prop_of @{{thm {qualified_thm}}}))))›\n\n"
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
    "cert_probe_theory_name",
    "write_cert_probe_theory",
    "write_theory_with_cert",
    "normalize_statement_repr",
    "statement_hash_of",
]
