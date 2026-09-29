"""Tests for ``trellis.checker.isabelle_session`` (checker B2a).

Exercise the Isabelle server TCP protocol client against a FAKE
``servers.db`` + an in-process fake TCP server speaking the wire-verified
framing (wire-verified against a live server), without spawning a real
``isabelle``:

* ``servers.db`` read (port/password) + first-line password auth;
* the message framing reader (single-line AND ``<decimal-length>\\n<body>``
  block framing);
* the async flow ``session_start`` → ``FINISHED {session_id}`` →
  ``use_theories`` → ``FINISHED {ok, nodes}``;
* the happy path (``ok:true``, ``failed:0``) ⇒ ``returncode == 0``;
* the fail path / NEGATIVE CONTROL (a node reporting ``failed > 0`` or an
  ``error`` message) ⇒ ``returncode != 0``;
* the soundness-cert parsing (``thm_oracles`` writeln → ``oracles``,
  ``thm_deps`` writeln → ``dependencies``, and ``theorem`` writeln →
  ``theorem_exists``).

The opt-in end-to-end test against a REAL ``isabelle server`` is marked
``@pytest.mark.isabelle_live`` and skipped by default.
"""

from __future__ import annotations

import json
import os
import socket
import sqlite3
import subprocess
import threading
import time
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional

import pytest

from trellis.checker import isabelle_session as isa
from trellis.checker.isabelle_session import (
    CheckOutcome,
    IsabelleSession,
    IsabelleSessionError,
    _parse_cert_from_writeln,
    write_theory_with_cert,
)


# --------------------------- framing-format helpers ---------------------------


def _frame_single(kind: str, payload: Optional[dict] = None) -> bytes:
    """Encode one reply as a single ``<KIND> <JSON>\\n`` line."""
    if payload is None:
        return (kind + "\n").encode("utf-8")
    return (f"{kind} {json.dumps(payload)}" + "\n").encode("utf-8")


def _frame_block(kind: str, payload: Optional[dict] = None) -> bytes:
    """Encode one reply using ``<decimal-length>\\n<body>\\n`` block framing.

    This is the framing the server uses for long / multi-line messages
    (Appendix A.3): a line containing only the byte-count, then exactly
    that many body bytes, then a trailing newline.
    """
    if payload is None:
        body = kind.encode("utf-8")
    else:
        body = f"{kind} {json.dumps(payload)}".encode("utf-8")
    framed = body + b"\n"  # real wire: the declared length INCLUDES this newline
    return f"{len(framed)}\n".encode("utf-8") + framed


# ------------------------------ fake TCP server ------------------------------


class _FakeIsabelleServer:
    """In-process TCP server that speaks the Isabelle wire framing.

    ``handler(command, arg)`` returns a list of raw byte frames to send in
    response to each received command line (after the password line). The
    test supplies a handler that scripts the ``session_start`` /
    ``use_theories`` async flow.
    """

    def __init__(
        self,
        handler: Callable[[str, Optional[dict]], List[bytes]],
    ) -> None:
        self._handler = handler
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._sock.bind(("127.0.0.1", 0))
        self._sock.listen(4)
        self.port = self._sock.getsockname()[1]
        self.password = "fake-password-uuid"
        self.received: List[str] = []
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._stop = False

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> None:
        self._stop = True
        try:
            self._sock.close()
        except OSError:
            pass

    def _serve(self) -> None:
        try:
            conn, _ = self._sock.accept()
        except OSError:
            return
        conn.settimeout(10.0)
        buf = b""
        first_line = True
        try:
            while not self._stop:
                while b"\n" not in buf:
                    try:
                        chunk = conn.recv(4096)
                    except (socket.timeout, OSError):
                        chunk = b""
                    if not chunk:
                        return
                    buf += chunk
                line, _, buf = buf.partition(b"\n")
                text = line.decode("utf-8", errors="replace")
                if first_line:
                    # Password line — validate but do not reply.
                    first_line = False
                    assert text == self.password, f"bad password: {text!r}"
                    continue
                self.received.append(text)
                head, _, tail = text.partition(" ")
                arg = json.loads(tail) if tail.strip() else None
                for frame in self._handler(head, arg):
                    conn.sendall(frame)
        finally:
            try:
                conn.close()
            except OSError:
                pass


# ------------------------------ scenario builders ------------------------------


def _node(
    *,
    theory_name: str,
    failed: int = 0,
    finished: int = 10,
    ok: bool = True,
    messages: Optional[List[dict]] = None,
) -> dict:
    return {
        "theory_name": theory_name,
        "node_name": f"/tmp/{theory_name}.thy",
        "status": {
            "failed": failed,
            "finished": finished,
            "ok": ok,
            "total": finished + failed,
            "consolidated": True,
        },
        "messages": messages or [],
        "exports": [],
    }


def _make_handler(
    *,
    session_id: str = "sess-123",
    use_theories_reply: Callable[[Optional[dict]], List[bytes]],
) -> Callable[[str, Optional[dict]], List[bytes]]:
    def handler(command: str, arg: Optional[dict]) -> List[bytes]:
        if command == "session_start":
            return [
                _frame_single("OK", {"task": "task-start"}),
                _frame_block("NOTE", {"task": "task-start", "message": "loading"}),
                _frame_block(
                    "FINISHED",
                    {"session_id": session_id, "tmp_dir": "/tmp/x", "task": "task-start"},
                ),
            ]
        if command == "use_theories":
            return use_theories_reply(arg)
        if command == "session_stop":
            return [_frame_single("OK", {"task": "task-stop"}),
                    _frame_single("FINISHED", {"task": "task-stop"})]
        return [_frame_single("ERROR", {"message": f"unexpected command {command}"})]

    return handler


@pytest.fixture
def fake_db(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Provide an isolated fake ``servers.db`` and pin the session to it.

    Returns a callable ``register(name, port, password)`` so a test can
    write the row the session will read.
    """
    db_path = tmp_path / "servers.db"
    con = sqlite3.connect(db_path)
    con.execute(
        "CREATE TABLE isabelle_servers (name TEXT PRIMARY KEY, port INTEGER, password TEXT)"
    )
    con.commit()
    con.close()
    os.chmod(db_path, 0o600)
    monkeypatch.setattr(isa, "servers_db_path", lambda: db_path)

    def register(name: str, port: int, password: str) -> None:
        con = sqlite3.connect(db_path)
        con.execute(
            "INSERT OR REPLACE INTO isabelle_servers (name, port, password) VALUES (?,?,?)",
            (name, port, password),
        )
        con.commit()
        con.close()

    return register


def _session_against_fake(
    name: str,
    server: _FakeIsabelleServer,
    register,
) -> IsabelleSession:
    """Build a session whose spawn/reap is stubbed to use the fake server.

    The real ``IsabelleSession`` spawns ``isabelle server`` and reaps it.
    Here we register the fake's row directly and neuter the spawn/reap so
    the protocol path runs unchanged against the in-process fake.
    """
    sess = IsabelleSession(name=name, start_timeout_secs=10.0)

    # ``start()`` deletes any stale row for this name (hygiene) BEFORE
    # spawning, so we (re)register the fake's row inside the stubbed spawn
    # — i.e. exactly where the real ``isabelle server`` would write it.
    def _fake_spawn() -> None:
        register(name, server.port, server.password)

    sess._spawn_server = _fake_spawn  # type: ignore[assignment]
    # Neuter the S7 version pin for the protocol-against-fake path: it
    # shells out to a real ``isabelle version`` we don't have here. The
    # dedicated ``test_start_rejects_old_isabelle_version`` exercises the
    # real ``_assert_version_pin`` separately.
    sess._assert_version_pin = lambda: None  # type: ignore[assignment]
    return sess


# ------------------------------ servers.db tests ------------------------------


def test_read_server_row_roundtrip(fake_db) -> None:
    fake_db("srvA", 4321, "pw-A")
    row = isa._read_server_row(isa.servers_db_path(), "srvA")
    assert row == (4321, "pw-A")
    # Absent name → None.
    assert isa._read_server_row(isa.servers_db_path(), "ghost") is None


def test_invalid_server_name_rejected() -> None:
    with pytest.raises(IsabelleSessionError) as exc:
        IsabelleSession(name="bad name with spaces")
    assert exc.value.kind == "spawn_failed"


# ------------------------------ framing reader ------------------------------


def test_framing_reader_handles_single_and_block(fake_db, monkeypatch) -> None:
    """The reader must decode both single-line and block-framed messages.

    The happy-path handler mixes ``_frame_single`` (session_start OK) and
    ``_frame_block`` (the multi-line NOTE + FINISHED), so a successful
    ``start()`` proves both framings round-trip.
    """

    def use_theories(arg):
        return [_frame_block("FINISHED", {"ok": True, "nodes": []})]

    server = _FakeIsabelleServer(_make_handler(use_theories_reply=use_theories))
    server.start()
    try:
        sess = _session_against_fake("frame-test", server, fake_db)
        # No real ``isabelle`` available in unit tests → neuter reap shell-out.
        monkeypatch.setattr(
            isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
        )
        sess.start()
        assert sess.session_id == "sess-123"
    finally:
        sess.close()
        server.stop()


# ------------------------------ happy path ------------------------------


def test_use_theories_happy_path_returncode_zero(fake_db, monkeypatch) -> None:
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    messages = [
        {"kind": "writeln", "message": "theorem triv: 1 + 1 = 2"},
        {"kind": "writeln", "message": "oracles:"},
        {
            "kind": "writeln",
            "message": "dependencies: 2\n    One_nat_def\n    add.right_neutral",
        },
    ]

    def use_theories(arg):
        assert arg["theories"] == ["Tablet_Triv"]
        node = _node(theory_name="Draft.Tablet_Triv", failed=0, ok=True, messages=messages)
        return [_frame_block("FINISHED", {"ok": True, "nodes": [node]})]

    server = _FakeIsabelleServer(_make_handler(use_theories_reply=use_theories))
    server.start()
    try:
        sess = _session_against_fake("happy", server, fake_db)
        sess.start()
        outcome = sess.check_theory(
            master_dir="/tmp", theory="Tablet_Triv", cert_theorem="triv"
        )
        assert outcome.ok is True
        assert outcome.failed == 0
        assert outcome.returncode == 0
        assert outcome.theorem_exists is True
        assert outcome.oracles == []
        assert outcome.dependencies == ["One_nat_def", "add.right_neutral"]
    finally:
        sess.close()
        server.stop()


# ------------------------------ fail / negative control ------------------------------


def test_use_theories_failed_node_returncode_nonzero(fake_db, monkeypatch) -> None:
    """NEGATIVE CONTROL: a node reporting ``failed > 0`` (the 1+1=3 shape)
    must yield ``returncode != 0`` and surface the error text on stderr."""
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    messages = [
        {"kind": "writeln", "message": "theorem bad: 1 + 1 = 3"},
        {"kind": "error", "message": "Failed to finish proof:\n 1. False"},
    ]

    def use_theories(arg):
        node = _node(theory_name="Draft.Tablet_Bad", failed=1, finished=9, ok=False,
                     messages=messages)
        return [_frame_block("FINISHED", {"ok": False, "nodes": [node]})]

    server = _FakeIsabelleServer(_make_handler(use_theories_reply=use_theories))
    server.start()
    try:
        sess = _session_against_fake("negctl", server, fake_db)
        sess.start()
        outcome = sess.check_theory(
            master_dir="/tmp", theory="Tablet_Bad", cert_theorem="bad"
        )
        assert outcome.ok is False
        assert outcome.failed == 1
        assert outcome.returncode != 0
        assert "Failed to finish proof" in outcome.stderr
    finally:
        sess.close()
        server.stop()


def test_use_theories_top_level_ok_false_returncode_nonzero(fake_db, monkeypatch) -> None:
    """Even when the per-node status is absent, a top-level ``ok:false``
    must not produce a success returncode."""
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )

    def use_theories(arg):
        # No nodes array (degenerate failure shape).
        return [_frame_single("FINISHED", {"ok": False})]

    server = _FakeIsabelleServer(_make_handler(use_theories_reply=use_theories))
    server.start()
    try:
        sess = _session_against_fake("okfalse", server, fake_db)
        sess.start()
        outcome = sess.check_theory(master_dir="/tmp", theory="Tablet_X")
        assert outcome.returncode != 0
    finally:
        sess.close()
        server.stop()


# ------------------------------ cert parsing ------------------------------


def test_cert_parser_clean_proof_empty_oracles() -> None:
    writeln = [
        "theorem triv: 1 + 1 = 2",
        "oracles:",
        "dependencies: 3\n    BitM_def\n    One_nat_def\n    add_Suc_right",
    ]
    (
        oracles, deps, exists, shyps, stmt, stmt_long, _res, _bnd, _struct, _typed
    ) = _parse_cert_from_writeln(
        writeln, "triv"
    )
    assert oracles == []
    assert deps == ["BitM_def", "One_nat_def", "add_Suc_right"]
    assert exists is True
    assert shyps == []
    assert stmt == ""
    assert stmt_long == ""


def test_cert_parser_sorry_surfaces_skip_proof() -> None:
    """A ``sorry`` proof taints the theorem with the ``skip_proof`` oracle
    (Isabelle's ``Pure.skip_proof``); the parser must surface it so the
    B2c-gate can reject it."""
    writeln = [
        "theorem viaSorry: 1 + 1 = 2",
        "oracles:\n    skip_proof",
    ]
    (
        oracles, deps, exists, _shyps, _stmt, _long, _res, _bnd, _struct, _typed
    ) = _parse_cert_from_writeln(
        writeln, "viaSorry"
    )
    assert oracles == ["skip_proof"]
    assert exists is True


def test_cert_parser_missing_theorem_when_name_absent() -> None:
    """When the named theorem's ``theorem`` line is absent (oops/elision),
    ``theorem_exists`` is False even if some unrelated theorem line appears."""
    writeln = ["theorem other: True", "oracles:"]
    (
        oracles, deps, exists, _shyps, _stmt, _long, _res, _bnd, _struct, _typed
    ) = _parse_cert_from_writeln(
        writeln, "triv"
    )
    assert exists is False


def test_cert_parser_handles_inline_oracle_names() -> None:
    writeln = ["oracles: skip_proof", "dependencies: 1\n    refl"]
    (
        oracles, deps, _exists, _shyps, _stmt, _long, _res, _bnd, _struct, _typed
    ) = _parse_cert_from_writeln(
        writeln, None
    )
    assert oracles == ["skip_proof"]
    assert deps == ["refl"]


# ------------------------------ theory authoring ------------------------------


def test_write_theory_with_cert_emits_cert_commands(tmp_path: Path) -> None:
    path = write_theory_with_cert(
        master_dir=tmp_path,
        theory="Tablet_Triv",
        body='lemma triv: "(1::nat)+1=2" by simp',
        cert_theorem="triv",
    )
    text = path.read_text()
    assert path.name == "Tablet_Triv.thy"
    assert "theory Tablet_Triv" in text
    assert "imports Main" in text
    assert 'lemma triv: "(1::nat)+1=2" by simp' in text
    assert "thm_oracles triv" in text
    assert "thm_deps triv" in text
    assert text.rstrip().endswith("end")


# ------------------------------ OPT-IN live harness ------------------------------


# ======================================================================
# Warm-prefix capability (Phase 1) — flag-gated OFF by default.
#
# The warm core (promote / evict / check_node / reconcile_accepted_base) is
# DORMANT unless ``warm_prefix_enabled``. These tests assert: (a) flag-OFF
# leaves the existing path byte-for-byte unchanged (no ``options`` on the
# ``session_start`` wire, warm methods refuse); (b) flag-ON drives the
# Option-A wire (consolidate-delay option, promote=use_theories,
# evict=purge_theories, reconcile diffs the accepted set); and (c) the
# warm-vs-cold cert byte-equality, as an opt-in live test.
# ======================================================================


from trellis.checker.isabelle_session import WarmCheckOutcome  # noqa: E402
from trellis.checker import isabelle_warm_config as warm_cfg  # noqa: E402


def _last_command_arg(server: "_FakeIsabelleServer", command: str) -> Optional[dict]:
    """Parse the JSON arg of the most-recent ``<command> {json}`` line the
    fake server received (the wire the session actually sent)."""
    for text in reversed(server.received):
        head, _, tail = text.partition(" ")
        if head == command:
            return json.loads(tail) if tail.strip() else None
    return None


def _warm_handler(
    *,
    session_id: str = "sess-warm",
    use_theories_reply: Callable[[Optional[dict]], List[bytes]],
) -> Callable[[str, Optional[dict]], List[bytes]]:
    """Like ``_make_handler`` but also answers ``purge_theories`` (the evict
    wire) with a single synchronous ``OK {purged, retained}`` reply — the REAL
    server protocol (``purge_theories`` is a synchronous command: one ``OK``
    result, NO async task / ``FINISHED``; live-confirmed 2026-06-23)."""

    def handler(command: str, arg: Optional[dict]) -> List[bytes]:
        if command == "session_start":
            return [
                _frame_single("OK", {"task": "t-start"}),
                _frame_block(
                    "FINISHED",
                    {"session_id": session_id, "tmp_dir": "/tmp/x", "task": "t-start"},
                ),
            ]
        if command == "use_theories":
            return use_theories_reply(arg)
        if command == "purge_theories":
            theories = (arg or {}).get("theories", [])
            # Synchronous: a single OK carrying the purge result (no FINISHED).
            return [
                _frame_block(
                    "OK",
                    {
                        "purged": [f"/tmp/{t}.thy" for t in theories],
                        "retained": [],
                    },
                ),
            ]
        if command == "session_stop":
            return [
                _frame_single("OK", {"task": "t-stop"}),
                _frame_single("FINISHED", {"task": "t-stop"}),
            ]
        return [_frame_single("ERROR", {"message": f"unexpected {command}"})]

    return handler


def _ok_node_reply(arg: Optional[dict]) -> List[bytes]:
    """A green single-node ``use_theories`` reply for the requested theory."""
    theory = (arg or {}).get("theories", ["Tablet_X"])[0]
    node = _node(theory_name=f"Draft.{theory}", failed=0, ok=True)
    return [_frame_block("FINISHED", {"ok": True, "nodes": [node]})]


def _warm_session_against_fake(
    name: str,
    server: "_FakeIsabelleServer",
    register,
    *,
    warm_prefix_enabled: bool,
    consolidate_delay: float = 0.05,
) -> IsabelleSession:
    sess = IsabelleSession(
        name=name,
        start_timeout_secs=10.0,
        warm_prefix_enabled=warm_prefix_enabled,
        consolidate_delay=consolidate_delay,
    )

    def _fake_spawn() -> None:
        register(name, server.port, server.password)

    sess._spawn_server = _fake_spawn  # type: ignore[assignment]
    sess._assert_version_pin = lambda: None  # type: ignore[assignment]
    return sess


def _write_thy(master: Path, theory: str, body: str = "") -> None:
    master.mkdir(parents=True, exist_ok=True)
    (master / f"{theory}.thy").write_text(
        f"theory {theory}\n  imports Main\nbegin\n\n{body}\n\nend\n",
        encoding="utf-8",
    )


# ----------------------- flag-OFF: behavior unchanged -----------------------


def test_flag_off_default_session_start_wire_unchanged(fake_db, monkeypatch) -> None:
    """The headline guarantee: with the warm flag OFF (the DEFAULT), the
    ``session_start`` wire is byte-identical to before the capability existed —
    ``{"session": <base>}`` with NO ``options`` key. A behavior-preserving
    proof that flag-OFF is a no-op vs the prior path."""
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    server = _FakeIsabelleServer(
        _warm_handler(use_theories_reply=_ok_node_reply, session_id="sess-123")
    )
    server.start()
    try:
        # Default constructor: no warm_prefix_enabled passed → resolves from the
        # env switch, which defaults OFF.
        sess = IsabelleSession(name="off-default", start_timeout_secs=10.0)
        assert sess.warm_prefix_enabled is False
        sess._spawn_server = lambda: fake_db(  # type: ignore[assignment]
            "off-default", server.port, server.password
        )
        sess._assert_version_pin = lambda: None  # type: ignore[assignment]
        sess.start()
        arg = _last_command_arg(server, "session_start")
        assert arg == {"session": sess.session}, arg
        assert "options" not in arg
    finally:
        sess.close()
        server.stop()


def test_flag_off_warm_methods_refuse(fake_db, monkeypatch) -> None:
    """Flag OFF ⇒ the warm methods RAISE (loud) rather than silently no-op, so
    a Phase-2 miswire that calls them on a cold session fails closed."""
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    server = _FakeIsabelleServer(
        _warm_handler(use_theories_reply=_ok_node_reply, session_id="sess-123")
    )
    server.start()
    try:
        sess = _warm_session_against_fake(
            "off-refuse", server, fake_db, warm_prefix_enabled=False
        )
        sess.start()
        for call in (
            lambda: sess.promote(master_dir="/tmp", theory="Tablet_A"),
            lambda: sess.evict(master_dir="/tmp", theory="Tablet_A"),
            lambda: sess.check_node(master_dir="/tmp", theory="Tablet_A"),
            lambda: sess.reconcile_accepted_base(master_dir="/tmp", accepted=["Tablet_A"]),
        ):
            with pytest.raises(IsabelleSessionError) as exc:
                call()
            assert exc.value.kind == "protocol_error"
        assert sess.promoted_theories == []
    finally:
        sess.close()
        server.stop()


def test_env_flag_toggles_enabled(monkeypatch) -> None:
    """The env switch (mirrors the other ``TRELLIS_ISABELLE_*`` overrides)
    resolves the default when no explicit value is passed; default OFF."""
    monkeypatch.delenv(warm_cfg.WARM_SESSION_ENV, raising=False)
    assert IsabelleSession(name="e-off").warm_prefix_enabled is False
    monkeypatch.setenv(warm_cfg.WARM_SESSION_ENV, "1")
    assert IsabelleSession(name="e-on").warm_prefix_enabled is True
    monkeypatch.setenv(warm_cfg.WARM_SESSION_ENV, "TRUE")
    assert IsabelleSession(name="e-on2").warm_prefix_enabled is True
    monkeypatch.setenv(warm_cfg.WARM_SESSION_ENV, "0")
    assert IsabelleSession(name="e-off2").warm_prefix_enabled is False
    # An explicit constructor arg always wins over the env.
    monkeypatch.setenv(warm_cfg.WARM_SESSION_ENV, "1")
    assert (
        IsabelleSession(name="e-explicit", warm_prefix_enabled=False).warm_prefix_enabled
        is False
    )


# ----------------------- flag-ON: Option-A wire -----------------------


def test_flag_on_session_start_carries_consolidate_delay(fake_db, monkeypatch) -> None:
    """Flag ON ⇒ ``session_start`` tunes ``headless_consolidate_delay`` (the
    Phase-0 knob) AND pins ``quick_and_dirty=false`` (so the warm load rejects
    ``sorry`` like the cold build). The base ``session`` is unchanged; only the
    ``options`` list is added."""
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    server = _FakeIsabelleServer(
        _warm_handler(use_theories_reply=_ok_node_reply, session_id="sess-warm")
    )
    server.start()
    try:
        sess = _warm_session_against_fake(
            "on-delay", server, fake_db, warm_prefix_enabled=True, consolidate_delay=0.05
        )
        sess.start()
        arg = _last_command_arg(server, "session_start")
        assert arg["session"] == sess.session
        assert arg["options"] == [
            "headless_consolidate_delay=0.05",
            "quick_and_dirty=false",
        ]
    finally:
        sess.close()
        server.stop()


def test_promote_warms_then_check_references_it(fake_db, monkeypatch, tmp_path) -> None:
    """promote → the sibling is recorded warm; a later check_node sends a
    single-theory ``use_theories`` (only the in-flight node re-elaborates, the
    promoted prefix stays warm)."""
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    _write_thy(tmp_path, "Tablet_Dep")
    _write_thy(tmp_path, "Tablet_InFlight")
    server = _FakeIsabelleServer(
        _warm_handler(use_theories_reply=_ok_node_reply, session_id="sess-warm")
    )
    server.start()
    try:
        sess = _warm_session_against_fake(
            "promote", server, fake_db, warm_prefix_enabled=True
        )
        sess.start()
        out = sess.promote(master_dir=str(tmp_path), theory="Tablet_Dep")
        assert out.ok is True
        assert sess.promoted_theories == ["Tablet_Dep"]
        # The promote went out as a use_theories of exactly [Tablet_Dep].
        promote_arg = _last_command_arg(server, "use_theories")
        assert promote_arg["theories"] == ["Tablet_Dep"]

        verdict = sess.check_node(master_dir=str(tmp_path), theory="Tablet_InFlight")
        assert isinstance(verdict, WarmCheckOutcome)
        assert verdict.ok is True
        assert verdict.outcome is not None
        # The in-flight check re-elaborates ONLY the in-flight node — under its
        # content-keyed H1 alias (a single-theory use_theories); the promoted
        # prefix is untouched (still warm).
        check_arg = _last_command_arg(server, "use_theories")
        assert len(check_arg["theories"]) == 1
        assert check_arg["theories"][0].startswith("Tablet_InFlight__In_"), check_arg
        assert sess.promoted_theories == ["Tablet_Dep"]
    finally:
        sess.close()
        server.stop()


def test_evict_then_repromote_no_residue(fake_db, monkeypatch, tmp_path) -> None:
    """evict → the theory is purged off the wire and forgotten; a re-promote of
    edited bytes re-adds it with the NEW sha (warm state == a clean re-promote,
    no stale residue)."""
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    _write_thy(tmp_path, "Tablet_Dep", body='lemma v1: "True" by simp')
    server = _FakeIsabelleServer(
        _warm_handler(use_theories_reply=_ok_node_reply, session_id="sess-warm")
    )
    server.start()
    try:
        sess = _warm_session_against_fake(
            "evict", server, fake_db, warm_prefix_enabled=True
        )
        sess.start()
        sess.promote(master_dir=str(tmp_path), theory="Tablet_Dep")
        sha_v1 = sess._promoted["Tablet_Dep"]

        sess.evict(master_dir=str(tmp_path), theory="Tablet_Dep")
        assert sess.promoted_theories == []
        # The evict issued purge_theories for exactly that theory.
        purge_arg = _last_command_arg(server, "purge_theories")
        assert purge_arg["theories"] == ["Tablet_Dep"]

        # Edit the sibling and re-promote: the new bytes get a new sha (no
        # residue from the prior version).
        _write_thy(tmp_path, "Tablet_Dep", body='lemma v2: "True \\<and> True" by simp')
        sess.promote(master_dir=str(tmp_path), theory="Tablet_Dep")
        sha_v2 = sess._promoted["Tablet_Dep"]
        assert sess.promoted_theories == ["Tablet_Dep"]
        assert sha_v2 != sha_v1
    finally:
        sess.close()
        server.stop()


def test_reconcile_promotes_retains_and_detects_content_change(
    fake_db, monkeypatch, tmp_path
) -> None:
    """reconcile_accepted_base holds the caller's REQUIRED set warm: promotes new
    members in order and re-promotes a content-changed one (sha differs).

    A theory that drops out of the required set is RETAINED, not evicted. The
    caller now passes the in-flight node's import cone, so "not required" means
    "this node does not import it" — it is unreachable from the node's
    elaboration and dropping it would only force a re-elaboration the next time
    some node does import it. Mirrors Lean, where `lake build Tablet.<Node>`
    leaves other nodes' `.olean` files alone. (Eviction was also a no-op on the
    wire: the public `purge_theories` discards its document edits on 2025-2.)
    """
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    for t in ("Tablet_Preamble", "Tablet_A", "Tablet_B"):
        _write_thy(tmp_path, t)
    server = _FakeIsabelleServer(
        _warm_handler(use_theories_reply=_ok_node_reply, session_id="sess-warm")
    )
    server.start()
    try:
        sess = _warm_session_against_fake(
            "reconcile", server, fake_db, warm_prefix_enabled=True
        )
        sess.start()

        # First reconcile: promote the whole accepted set, in order.
        sess.reconcile_accepted_base(
            master_dir=str(tmp_path), accepted=["Tablet_Preamble", "Tablet_A", "Tablet_B"]
        )
        assert sess.promoted_theories == ["Tablet_A", "Tablet_B", "Tablet_Preamble"]
        sha_a0 = sess._promoted["Tablet_A"]

        # B drops out of the required set; A changes content. Reconcile again.
        _write_thy(tmp_path, "Tablet_A", body='lemma changed: "True" by simp')
        sess.reconcile_accepted_base(
            master_dir=str(tmp_path), accepted=["Tablet_Preamble", "Tablet_A"]
        )
        # B RETAINED (unreachable from this node, so harmless and worth keeping
        # warm); A re-promoted with new bytes; Preamble untouched.
        assert sess.promoted_theories == ["Tablet_A", "Tablet_B", "Tablet_Preamble"]
        assert sess._promoted["Tablet_A"] != sha_a0
        # An idempotent reconcile (no disk change) promotes nothing new.
        sent_before = len(server.received)
        sess.reconcile_accepted_base(
            master_dir=str(tmp_path), accepted=["Tablet_Preamble", "Tablet_A"]
        )
        # No use_theories/purge issued (all shas match) — only the count is
        # unchanged because nothing was sent.
        assert len(server.received) == sent_before
    finally:
        sess.close()
        server.stop()


def test_reconcile_total_budget_shares_across_promotes(monkeypatch, tmp_path) -> None:
    """``total_budget_secs`` is ONE budget shared across the whole reconcile
    (tranche 4): each member promote gets the REMAINING time — never a fresh
    per-member ``timeout_secs`` — and exhausting the budget before a needed
    promote raises ``timed_out`` while keeping the already-promoted members
    recorded (the next reconcile resumes, not restarts)."""
    sess = IsabelleSession(name="budget-share", session="HOL", warm_prefix_enabled=True)
    monkeypatch.setattr(sess, "_require_warm", lambda _op: None)
    for t in ("Tablet_A", "Tablet_B", "Tablet_C"):
        _write_thy(tmp_path, t)

    clock = {"now": 1000.0}
    monkeypatch.setattr(isa.time, "monotonic", lambda: clock["now"])

    promoted_with: List[tuple] = []

    def fake_promote(*, master_dir, theory, timeout_secs):
        promoted_with.append((theory, timeout_secs))
        clock["now"] += 100.0  # each promote consumes 100 s of the budget
        sess._promoted[theory] = sess._theory_sha(master_dir, theory)

    monkeypatch.setattr(sess, "promote", fake_promote)

    with pytest.raises(IsabelleSessionError) as exc:
        sess.reconcile_accepted_base(
            master_dir=str(tmp_path),
            accepted=["Tablet_A", "Tablet_B", "Tablet_C"],
            total_budget_secs=180.0,
        )
    assert exc.value.kind == "timed_out"
    assert "Tablet_C" in exc.value.message  # names the member it ran out before
    # The PRE-DISPATCH exhaustion carries the distinct type (no promote task
    # in flight — the warm gate keeps the session on this one; a promote's
    # own client-side timeout stays the base class and is reaped).
    from trellis.checker.isabelle_session import IsabelleReconcileBudgetExhausted

    assert isinstance(exc.value, IsabelleReconcileBudgetExhausted)

    # A got the full budget, B only the REMAINING 80 s (shared, not
    # multiplied); C was never promoted.
    assert [t for t, _ in promoted_with] == ["Tablet_A", "Tablet_B"]
    assert promoted_with[0][1] == pytest.approx(180.0)
    assert promoted_with[1][1] == pytest.approx(80.0)
    # The finished promotes are RETAINED (resume, don't restart): a follow-up
    # reconcile with a fresh budget only has C left to do.
    promoted_with.clear()
    sess.reconcile_accepted_base(
        master_dir=str(tmp_path),
        accepted=["Tablet_A", "Tablet_B", "Tablet_C"],
        total_budget_secs=180.0,
    )
    assert [t for t, _ in promoted_with] == ["Tablet_C"]

    # Without ``total_budget_secs`` the legacy per-member ``timeout_secs``
    # applies verbatim to every promote (the failure-open advisory path).
    sess._promoted.clear()
    promoted_with.clear()
    sess.reconcile_accepted_base(
        master_dir=str(tmp_path),
        accepted=["Tablet_A", "Tablet_B"],
        timeout_secs=55.0,
    )
    assert [ts for _, ts in promoted_with] == [55.0, 55.0]


def test_purge_theories_reads_single_synchronous_ok(fake_db, monkeypatch, tmp_path) -> None:
    """``purge_theories`` is a SYNCHRONOUS server command: it replies with ONE
    ``OK {purged, retained}`` and NO async task / ``FINISHED`` (unlike
    ``use_theories``/``session_start``). ``_purge_theories`` must read that
    single ``OK`` and return — NOT block waiting for a ``FINISHED`` that never
    comes (the live-confirmed hang the warm work would have hit). A follow-up op
    must then succeed (the socket is in sync, not desynced by an unread reply)."""
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    _write_thy(tmp_path, "Tablet_Dep")
    server = _FakeIsabelleServer(
        _warm_handler(use_theories_reply=_ok_node_reply, session_id="sess-warm")
    )
    server.start()
    try:
        sess = _warm_session_against_fake("purge-sync", server, fake_db, warm_prefix_enabled=True)
        sess.start()
        sess.promote(master_dir=str(tmp_path), theory="Tablet_Dep")
        # The evict drives purge_theories; with the single-OK stub this returns
        # promptly (no FINISHED to await). A short timeout proves it does not
        # block on a missing terminal reply.
        sess.evict(master_dir=str(tmp_path), theory="Tablet_Dep", timeout_secs=5.0)
        assert sess.promoted_theories == []
        purge_arg = _last_command_arg(server, "purge_theories")
        assert purge_arg["theories"] == ["Tablet_Dep"]
        # The socket is in sync: a subsequent check round-trips cleanly.
        _write_thy(tmp_path, "Tablet_After")
        verdict = sess.check_node(master_dir=str(tmp_path), theory="Tablet_After")
        assert verdict.ok is True
    finally:
        sess.close()
        server.stop()


def test_purge_inflight_purges_only_the_node(fake_db, monkeypatch, tmp_path) -> None:
    """H1: ``purge_inflight`` purges ONLY the in-flight node off the warm document
    (the cert probe uses a per-call unique name and is never purged — purging a
    probe that imports the node also invalidates the node), leaving the promoted
    (accepted) prefix untouched."""
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    _write_thy(tmp_path, "Tablet_Dep")
    server = _FakeIsabelleServer(
        _warm_handler(use_theories_reply=_ok_node_reply, session_id="sess-warm")
    )
    server.start()
    try:
        sess = _warm_session_against_fake("purge-inflight", server, fake_db, warm_prefix_enabled=True)
        sess.start()
        sess.promote(master_dir=str(tmp_path), theory="Tablet_Dep")  # accepted sibling
        sess.purge_inflight(master_dir=str(tmp_path), theory="Tablet_InFlight", timeout_secs=5.0)
        purge_arg = _last_command_arg(server, "purge_theories")
        assert purge_arg["theories"] == ["Tablet_InFlight"]
        # The accepted-sibling prefix is NOT touched (the in-flight node was
        # never in it).
        assert sess.promoted_theories == ["Tablet_Dep"]
    finally:
        sess.close()
        server.stop()


def _commands_in_order(server: "_FakeIsabelleServer") -> List[str]:
    """The command verbs the fake server received, in wire order."""
    return [text.partition(" ")[0] for text in server.received]


def _use_theories_targets(server: "_FakeIsabelleServer") -> List[str]:
    """The ``theories[0]`` each ``use_theories`` the fake server received drove
    against, in wire order — i.e. the actual theory names elaborated."""
    out: List[str] = []
    for text in server.received:
        head, _, tail = text.partition(" ")
        if head == "use_theories" and tail.strip():
            ths = json.loads(tail).get("theories") or []
            if ths:
                out.append(ths[0])
    return out


def test_fresh_inflight_theory_changed_content_elaborates_fresh_alias(
    fake_db, monkeypatch, tmp_path
) -> None:
    """H1 (the bug fix): a held-open warm session re-checking the SAME in-flight
    node after a proof-REPLACING edit elaborates a DIFFERENT, content-keyed alias
    theory (``<node>__In_<sha>``) — a fresh name forces a fresh elaboration of
    the current bytes (a purge+reload of the same name corrupts the held-open
    document, so a fresh name is the mechanism). UNCHANGED bytes reuse the SAME
    alias (no re-elaboration churn)."""
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    _write_thy(tmp_path, "Tablet_InFlight", body='lemma a: "(1::nat)+1=2" by simp')
    server = _FakeIsabelleServer(
        _warm_handler(use_theories_reply=_ok_node_reply, session_id="sess-warm")
    )
    server.start()
    try:
        sess = _warm_session_against_fake(
            "h1-alias", server, fake_db, warm_prefix_enabled=True
        )
        sess.start()
        sess.check_node(master_dir=str(tmp_path), theory="Tablet_InFlight")
        alias_a = sess.inflight_alias_of["Tablet_InFlight"]
        # The alias is a CONTENT-KEYED rename of the node, never the bare name.
        assert alias_a.startswith("Tablet_InFlight__In_"), alias_a
        # Its .thy was staged with ONLY the header renamed to the alias (the bare
        # original ``theory Tablet_InFlight`` header line is gone; the rest of the
        # body — imports/proof — is byte-identical to the source).
        src_text = (tmp_path / "Tablet_InFlight.thy").read_text(encoding="utf-8")
        alias_text = (tmp_path / f"{alias_a}.thy").read_text(encoding="utf-8")
        assert alias_text.startswith(f"theory {alias_a}\n"), alias_text[:60]
        assert "theory Tablet_InFlight\n" not in alias_text
        # Body after the header line is identical between source and alias.
        assert alias_text.split("\n", 1)[1] == src_text.split("\n", 1)[1]

        # Same bytes again ⇒ SAME alias, reused (the staged file already exists,
        # so no rewrite; the warm doc reuses the resident copy).
        sess.check_node(master_dir=str(tmp_path), theory="Tablet_InFlight")
        assert sess.inflight_alias_of["Tablet_InFlight"] == alias_a

        # The worker REPLACES the proof (clean → sorry): different bytes ⇒ a NEW
        # content-keyed alias ⇒ the next use_theories drives the NEW name.
        _write_thy(tmp_path, "Tablet_InFlight", body='lemma a: "(1::nat)+1=2" sorry')
        sess.check_node(master_dir=str(tmp_path), theory="Tablet_InFlight")
        alias_b = sess.inflight_alias_of["Tablet_InFlight"]
        assert alias_b != alias_a, "changed bytes must map to a new alias"
        assert alias_b.startswith("Tablet_InFlight__In_")
        # The superseded alias file was removed; the live one persists.
        assert not (tmp_path / f"{alias_a}.thy").exists()
        assert (tmp_path / f"{alias_b}.thy").exists()

        # The wire elaborated: alias_a (check1), alias_a (check2 reuse), alias_b.
        targets = _use_theories_targets(server)
        assert targets == [alias_a, alias_a, alias_b], targets
        # No purge_theories of the in-flight node (purge+reload corrupts the doc).
        assert "purge_theories" not in _commands_in_order(server)
    finally:
        sess.close()
        server.stop()


def test_check_node_relabels_alias_outcome_to_node(fake_db, monkeypatch, tmp_path) -> None:
    """The alias is an internal detail: ``check_node`` relabels the outcome's
    ``theory_name`` back to the caller's node so callers never see ``__In_``."""
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    _write_thy(tmp_path, "Tablet_InFlight", body='lemma a: "(1::nat)+1=2" by simp')

    def _reply_echo_theory(arg):
        theory = (arg or {}).get("theories", ["Tablet_X"])[0]
        node = _node(theory_name=f"Draft.{theory}", failed=0, ok=True)
        return [_frame_block("FINISHED", {"ok": True, "nodes": [node]})]

    server = _FakeIsabelleServer(
        _warm_handler(use_theories_reply=_reply_echo_theory, session_id="sess-warm")
    )
    server.start()
    try:
        sess = _warm_session_against_fake(
            "h1-relabel", server, fake_db, warm_prefix_enabled=True
        )
        sess.start()
        verdict = sess.check_node(master_dir=str(tmp_path), theory="Tablet_InFlight")
        assert verdict.ok is True
        # The wire drove the alias, but the outcome is relabeled to the node.
        assert verdict.outcome.theory_name == "Tablet_InFlight"
        assert _use_theories_targets(server)[0].startswith("Tablet_InFlight__In_")
    finally:
        sess.close()
        server.stop()


def test_fresh_inflight_theory_cold_flag_off_returns_original_name(
    fake_db, monkeypatch, tmp_path
) -> None:
    """The cold (flag-OFF) path is byte-identical to before H1: no alias is
    staged and the original node name is elaborated (a cold session re-builds
    the graph from a fresh process; there is no held-open residency to stale)."""
    monkeypatch.setattr(
        isa.subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    )
    _write_thy(tmp_path, "Tablet_Cold", body='lemma c: "(1::nat)+1=2" by simp')
    server = _FakeIsabelleServer(
        _warm_handler(use_theories_reply=_ok_node_reply, session_id="sess-warm")
    )
    server.start()
    try:
        sess = _warm_session_against_fake(
            "h1-cold", server, fake_db, warm_prefix_enabled=False
        )
        sess.start()
        # The pure helper returns the original name and stages no alias.
        assert sess.fresh_inflight_theory(
            master_dir=str(tmp_path), theory="Tablet_Cold"
        ) == "Tablet_Cold"
        assert list(tmp_path.glob("*__In_*.thy")) == []
        # And check_theory drives exactly the node name (cold wire unchanged).
        sess.check_theory(master_dir=str(tmp_path), theory="Tablet_Cold")
        assert _use_theories_targets(server) == ["Tablet_Cold"]
        assert "purge_theories" not in _commands_in_order(server)
    finally:
        sess.close()
        server.stop()


@pytest.mark.isabelle_live
def test_flag_off_session_start_wire_identical_live(tmp_path: Path) -> None:
    """OPT-IN LIVE no-op proof: against a REAL ``isabelle server``, a flag-OFF
    session checks a trivial node EXACTLY as the pre-capability code did
    (``ok:true``, empty oracles). The deterministic fake-server test
    (``test_flag_off_default_session_start_wire_unchanged``) already proves the
    wire is byte-identical; this confirms the real server accepts it unchanged.
    """
    isabelle_bin = isa.isabelle_bin()
    if not Path(isabelle_bin).exists():
        pytest.skip(f"isabelle binary not found at {isabelle_bin}")
    name = f"trellis-warmoff-live-{os.getpid()}"
# Base session: these live tests check the SESSION/CERT PROTOCOL on trivial
# theories that need only `Main`, and they pass no `session_dirs`. The
# default base (`Tablet_Base`) is DEFINED only in a scaffolded repo's
# `isabelle/ROOT`, so resolving it here would fail `session_start` with
# "Undefined session(s)" once the warm base is present in the system heaps.
# Pin the built-in `HOL` image, which needs no session dir.
    sess = IsabelleSession(
        name=name, session="HOL", start_timeout_secs=600.0, warm_prefix_enabled=False
    )
    assert sess.warm_prefix_enabled is False
    try:
        sess.start()
        write_theory_with_cert(
            master_dir=tmp_path,
            theory="Tablet_Triv",
            body='lemma triv: "(1::nat)+1=2" by simp',
            cert_theorem="triv",
        )
        good = sess.check_theory(
            master_dir=str(tmp_path), theory="Tablet_Triv", cert_theorem="triv",
            timeout_secs=600.0,
        )
        assert good.ok is True and good.returncode == 0 and good.oracles == []
    finally:
        sess.close()
        try:
            subprocess.run([isabelle_bin, "server", "-n", name, "-x"],
                           capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.SubprocessError):
            pass


@pytest.mark.isabelle_live
def test_warm_cert_equals_cold_cert_live(tmp_path: Path) -> None:
    """THE Phase-0 GO property, as an opt-in live test: the soundness cert a
    WARM-prefix check produces is byte-identical to a COLD elaboration of the
    same accepted node.

    Closed node: ``(\\<Sum>i=0..n. 2*i) = n*(n+1)`` by induction. We extract the
    cert (oracles / dependencies / TRELLIS_SHYPS / TRELLIS_STMT[_LONG]) two
    ways and assert the parsed cert fields match exactly:
      COLD  — a flag-OFF session, single check of the probe (cold node graph);
      WARM  — a flag-ON session, the node promoted FIRST (warm prefix), then the
              probe checked against it.
    Run ONLY in throwaway scratch:  pytest -m isabelle_live tests/test_isabelle_session.py
    """
    isabelle_bin = isa.isabelle_bin()
    if not Path(isabelle_bin).exists():
        pytest.skip(f"isabelle binary not found at {isabelle_bin}")

    from trellis.checker.isabelle_session import (
        write_cert_probe_theory,
        cert_probe_theory_name,
    )

    node = "Tablet_CertNode"
    principal = "certnode"
    # The accepted, closed node theory + the checker-owned cert probe.
    (tmp_path / f"{node}.thy").write_text(
        f"theory {node}\n  imports Complex_Main\nbegin\n\n"
        f"lemma {principal}:\n  fixes n :: nat\n"
        f'  shows "(\\<Sum>i=0..n. (2::nat)*i) = n*(n+1)"\n'
        f"  by (induction n) (auto simp: algebra_simps)\n\nend\n",
        encoding="utf-8",
    )
    write_cert_probe_theory(tmp_path, node, f"{node}.{principal}")
    probe = cert_probe_theory_name(node)

    def _cert_tuple(outcome):
        return (
            outcome.oracles,
            outcome.dependencies,
            outcome.extra_shyps,
            outcome.statement_hash,
            outcome.statement_repr,
            outcome.statement_repr_long,
        )

    # COLD: flag-OFF session, just check the probe (imports the node; cold graph).
    cold_name = f"trellis-cert-cold-{os.getpid()}"
# Base session: these live tests check the SESSION/CERT PROTOCOL on trivial
# theories that need only `Main`, and they pass no `session_dirs`. The
# default base (`Tablet_Base`) is DEFINED only in a scaffolded repo's
# `isabelle/ROOT`, so resolving it here would fail `session_start` with
# "Undefined session(s)" once the warm base is present in the system heaps.
# Pin the built-in `HOL` image, which needs no session dir.
    cold_sess = IsabelleSession(
        name=cold_name, session="HOL", start_timeout_secs=600.0, warm_prefix_enabled=False
    )
    try:
        cold_sess.start()
        # The probe references the principal by its SERVER-constructed qualified
        # name internally; the raw probe outcome's ``theorem_exists`` is not the
        # gate signal here (the 2025-2 ``thm_oracles``/``thm_deps`` output emits
        # no bare ``theorem <name>:`` line). The cert FIELDS are what the gate
        # reads, and what warm==cold must hold over.
        cold = cold_sess.check_theory(
            master_dir=str(tmp_path), theory=probe, cert_theorem=None,
            timeout_secs=600.0,
        )
    finally:
        cold_sess.close()
        try:
            subprocess.run([isabelle_bin, "server", "-n", cold_name, "-x"],
                           capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.SubprocessError):
            pass

    # WARM: flag-ON session, promote the node FIRST (warm prefix), then probe.
    warm_name = f"trellis-cert-warm-{os.getpid()}"
    warm_sess = IsabelleSession(
        name=warm_name, session="HOL", start_timeout_secs=600.0, warm_prefix_enabled=True
    )
    try:
        warm_sess.start()
        warm_sess.promote(master_dir=str(tmp_path), theory=node, timeout_secs=600.0)
        warm_verdict = warm_sess.check_node(
            master_dir=str(tmp_path), theory=probe, cert_theorem=None,
            timeout_secs=600.0,
        )
        warm = warm_verdict.outcome
    finally:
        warm_sess.close()
        try:
            subprocess.run([isabelle_bin, "server", "-n", warm_name, "-x"],
                           capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.SubprocessError):
            pass

    # The cold cert must itself be a real, non-trivial certificate (so the
    # equality below is meaningful, not two empty tuples matching).
    assert cold.oracles == [], f"cold node must be oracle-clean, got {cold.oracles}"
    assert cold.extra_shyps == [], f"cold node must have no dangling shyps, got {cold.extra_shyps}"
    assert len(cold.dependencies) > 100, f"expected the full dep set, got {len(cold.dependencies)}"
    assert cold.statement_hash != "", "cold statement must hash (non-empty repr)"
    # Phase-0 verified statement (strong correctness anchor).
    assert cold.statement_repr == "sum ((*) 2) {0..?n} = ?n * (?n + 1)", cold.statement_repr
    assert warm is not None
    # THE crux property: warm cert == cold cert, field-for-field.
    assert _cert_tuple(warm) == _cert_tuple(cold), (
        f"warm cert diverged from cold:\n warm={_cert_tuple(warm)}\n cold={_cert_tuple(cold)}"
    )


@pytest.mark.isabelle_live
def test_live_isabelle_triv_and_negative_control(tmp_path: Path) -> None:
    """End-to-end against a REAL ``isabelle server`` (opt-in; skip-by-default).

    Run ONLY in throwaway scratch, never during a live Lean run:
        pytest -m isabelle_live tests/test_isabelle_session.py

    * a real ``Tablet_Triv.thy`` (``lemma triv: "(1::nat)+1=2" by simp``)
      checks ``ok:true`` with EMPTY ``oracles``;
    * the NEGATIVE control ``"(1::nat)+1=3"`` checks ``ok:false``.

    The server is reaped BY NAME on exit.
    """
    isabelle_bin = isa.isabelle_bin()
    if not Path(isabelle_bin).exists():
        pytest.skip(f"isabelle binary not found at {isabelle_bin}")

    name = f"trellis-b2a-livetest-{os.getpid()}"
    # Generous budgets: the HOL session start + a real ``use_theories``
    # elaboration can be slow when co-resident with another heavy build
    # (the negative control's ``simp`` on a false goal is especially slow).
# Base session: these live tests check the SESSION/CERT PROTOCOL on trivial
# theories that need only `Main`, and they pass no `session_dirs`. The
# default base (`Tablet_Base`) is DEFINED only in a scaffolded repo's
# `isabelle/ROOT`, so resolving it here would fail `session_start` with
# "Undefined session(s)" once the warm base is present in the system heaps.
# Pin the built-in `HOL` image, which needs no session dir.
    sess = IsabelleSession(name=name, session="HOL", start_timeout_secs=600.0)
    try:
        sess.start()
        # Positive: 1+1=2.
        write_theory_with_cert(
            master_dir=tmp_path,
            theory="Tablet_Triv",
            body='lemma triv: "(1::nat)+1=2" by simp',
            cert_theorem="triv",
        )
        good = sess.check_theory(
            master_dir=str(tmp_path), theory="Tablet_Triv", cert_theorem="triv",
            timeout_secs=600.0,
        )
        assert good.ok is True, good
        assert good.failed == 0, good
        assert good.returncode == 0
        assert good.oracles == [], f"clean proof must have empty oracles, got {good.oracles}"
        assert good.theorem_exists is True
        assert good.dependencies, "thm_deps must surface a non-empty dependency set"

        # Negative control: 1+1=3.
        write_theory_with_cert(
            master_dir=tmp_path,
            theory="Tablet_Bad",
            body='lemma bad: "(1::nat)+1=3" by simp',
            cert_theorem="bad",
        )
        bad = sess.check_theory(
            master_dir=str(tmp_path), theory="Tablet_Bad", cert_theorem="bad",
            timeout_secs=600.0,
        )
        assert bad.ok is False, bad
        assert bad.returncode != 0
    finally:
        sess.close()
        # Defensive double-reap by name + row cleanup.
        try:
            subprocess.run([isabelle_bin, "server", "-n", name, "-x"],
                           capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.SubprocessError):
            pass


@pytest.mark.isabelle_live
def test_warm_inflight_content_gate_stale_to_fresh_live(tmp_path: Path) -> None:
    """H1 (warm-residency-staleness) REGRESSION, live against a REAL
    ``isabelle server``.

    Reproduces the ``Subcritical`` live-halt scenario on the held-open WARM
    document: a worker re-checks the SAME in-flight node after a proof-REPLACING
    edit (clean → ``sorry``). Without the content-gate the warm session reads the
    residency-stale CLEAN version (``oracles==[]``, ``theorem_exists`` true); with
    it the in-flight theory is purged + re-loaded so the cert reflects the
    CURRENT ``sorry`` (``skip_proof`` in ``oracles``, ``theorem_exists`` false).

    Also asserts the same-content re-check does NOT needlessly re-elaborate
    (the content sha-gate skips it — no purge, the recorded sha is stable).

    Run ONLY in throwaway scratch, never during a live run:
        pytest -m isabelle_live tests/test_isabelle_session.py
    """
    isabelle_bin = isa.isabelle_bin()
    if not Path(isabelle_bin).exists():
        pytest.skip(f"isabelle binary not found at {isabelle_bin}")

    theory = "Tablet_Subcrit"
    principal = "subcrit"

    def _write(body: str) -> None:
        write_theory_with_cert(
            master_dir=tmp_path,
            theory=theory,
            body=body,
            cert_theorem=principal,
        )

    # Content A: a CLEAN proof.
    _write(f'lemma {principal}: "(1::nat)+1=2" by simp')

    # Pin HOL (universally prebuilt, needs no scaffold dirs) so the test starts
    # on any host; the warm flag still pins quick_and_dirty=false.
    name = f"trellis-h1-stale-live-{os.getpid()}"
    sess = IsabelleSession(
        name=name, session="HOL", start_timeout_secs=600.0, warm_prefix_enabled=True
    )
    try:
        sess.start()

        # (1) check A on the held-open warm document → CLEAN cert.
        a1 = sess.check_node(
            master_dir=str(tmp_path), theory=theory, cert_theorem=principal,
            timeout_secs=600.0,
        )
        out_a = a1.outcome
        assert out_a is not None
        assert out_a.returncode == 0, out_a
        assert out_a.oracles == [], f"clean A must be oracle-free, got {out_a.oracles}"
        assert out_a.theorem_exists is True, out_a
        # The node elaborated under a content-keyed in-flight alias, and the
        # outcome was relabeled back to the node name (alias is internal).
        alias_a = sess.inflight_alias_of[theory]
        assert alias_a.startswith(f"{theory}__In_"), alias_a
        assert out_a.theory_name == theory

        # (1b) a SAME-content re-check reuses the SAME alias (no churn).
        a2 = sess.check_node(
            master_dir=str(tmp_path), theory=theory, cert_theorem=principal,
            timeout_secs=600.0,
        )
        assert a2.outcome is not None and a2.outcome.oracles == []
        assert sess.inflight_alias_of[theory] == alias_a, "same bytes ⇒ same alias"

        # (2) the worker REPLACES the proof with a ``sorry`` (content B).
        _write(f'lemma {principal}: "(1::nat)+1=2" sorry')

        # (3) re-check the SAME node → the verdict MUST reflect B, NOT the stale
        # clean A. The stale-read signature is "clean proven theorem" (oracles
        # empty AND theorem_exists AND rc==0); B must NOT present that. Under the
        # warm quick_and_dirty=false a sorry surfaces skip_proof and/or a failed
        # build — either way it is detectably non-clean.
        b = sess.check_node(
            master_dir=str(tmp_path), theory=theory, cert_theorem=principal,
            timeout_secs=600.0,
        )
        out_b = b.outcome
        assert out_b is not None
        alias_b = sess.inflight_alias_of[theory]
        assert alias_b != alias_a, "changed bytes must map to a new alias"
        stale_clean = (
            out_b.oracles == [] and out_b.theorem_exists and out_b.returncode == 0
        )
        assert not stale_clean, (
            f"H1 STALE: the sorry node B read as a clean proven theorem (a stale "
            f"read of clean A): rc={out_b.returncode} oracles={out_b.oracles} "
            f"theorem_exists={out_b.theorem_exists}"
        )
        # Specifically, the current sorry's skip_proof oracle must surface.
        assert "skip_proof" in out_b.oracles, (
            f"expected the current sorry's skip_proof oracle, got {out_b.oracles}"
        )
    finally:
        sess.close()
        try:
            subprocess.run([isabelle_bin, "server", "-n", name, "-x"],
                           capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.SubprocessError):
            pass


@pytest.mark.isabelle_live
def test_warm_cert_probe_stale_to_fresh_live(tmp_path: Path) -> None:
    """H1 REGRESSION on the CERT path (``run_cert_probe`` — the channel the live
    ``Subcritical`` closure cross-check halted on), live against a real server.

    The cross-check compares the warm cert (``run_cert_probe``) against a cold
    one. Before H1 a worker proof-replacing edit (clean→``sorry``) let the warm
    cert read residency-stale CLEAN while cold read the ``sorry`` — a divergence
    that HALTED. After H1 the warm cert reflects the CURRENT content (the node is
    built + probed under a content-keyed alias), so warm == cold and there is no
    spurious halt. We assert the warm cert flips clean→``skip_proof`` across the
    edit AND that the alias name does NOT leak into ``statement_repr_long`` (which
    the cross-check compares — an un-normalized alias would itself trip a halt).

    Run ONLY in throwaway scratch:
        pytest -m isabelle_live tests/test_isabelle_session.py
    """
    from trellis.atomic_actions import isabelle_observations as obs

    isabelle_bin = isa.isabelle_bin()
    if not Path(isabelle_bin).exists():
        pytest.skip(f"isabelle binary not found at {isabelle_bin}")

    node, principal = "Tablet_Subcrit", "subcrit"
    # The node defines a LOCAL constant used in its statement so the long-name
    # print exercises the alias-qualifier de-leak.
    def _write(proof: str) -> None:
        (tmp_path / f"{node}.thy").write_text(
            f"theory {node}\n  imports Main\nbegin\n\n"
            f'definition cg :: "nat \\<Rightarrow> bool" where '
            f'"cg n \\<longleftrightarrow> n = n"\n\n'
            f'lemma {principal}: "cg n" {proof}\n\nend\n',
            encoding="utf-8",
        )

    # Clean A.
    _write("by (simp add: cg_def)")
    name = f"trellis-h1-probe-live-{os.getpid()}"
    sess = IsabelleSession(
        name=name, session="HOL", start_timeout_secs=600.0, warm_prefix_enabled=True
    )
    try:
        sess.start()
        a = obs.run_cert_probe(
            sess, master_dir=str(tmp_path), theory=node, cert_theorem=principal,
            timeout_secs=600.0,
        )
        assert a.oracles == [], f"clean A cert must be oracle-free, got {a.oracles}"
        assert a.theorem_exists is True
        # The long-name print must carry the REAL node qualifier, never an alias.
        assert "__In_" not in a.statement_repr_long, a.statement_repr_long
        assert a.statement_repr_long.startswith(f"{node}."), a.statement_repr_long
        long_a = a.statement_repr_long

        # Worker replaces the proof with a sorry (content B).
        _write("sorry")
        b = obs.run_cert_probe(
            sess, master_dir=str(tmp_path), theory=node, cert_theorem=principal,
            timeout_secs=600.0,
        )
        # THE bug: the warm cert must reflect B's sorry, not stale clean A.
        assert "skip_proof" in b.oracles, (
            f"H1 STALE: warm cert read clean A for a now-sorry node: {b.oracles}"
        )
        # And still no alias leak in the compared long-name field.
        assert "__In_" not in b.statement_repr_long, b.statement_repr_long
        # The statement itself is unchanged (same lemma), so the SHORT repr/hash
        # match clean A — only the oracle set flips. Confirms the de-leak keeps
        # statement_repr_long node-stable across the edit.
        assert b.statement_repr_long == long_a, (b.statement_repr_long, long_a)
    finally:
        sess.close()
        try:
            subprocess.run([isabelle_bin, "server", "-n", name, "-x"],
                           capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.SubprocessError):
            pass


def test_cut_walk_ml_is_checker_owned_and_cuts_at_declared_boundaries() -> None:
    """The cut-walk probe text attributes oracles to the proof that introduced
    them, so a node whose only unproven dependency is a declared child is
    distinguishable from one whose own proof is unfinished.

    `Thm_Deps.all_oracles` unions the whole transitive graph, so it reports
    `skip_proof` for BOTH — which is why the pre-existing gate had to reject
    every ancestor of an open node. Subtracting the child's oracle NAMES is not a
    fix and is unsound: two independent `skip_proof` occurrences are
    indistinguishable by name, so `{skip_proof} - {skip_proof} = {}` would accept
    a parent that also has its own `sorry`. Ownership must come from structure.
    """
    from trellis.checker.isabelle_session import (
        TRELLIS_BOUNDARY_TAG,
        TRELLIS_RESIDUAL_TAG,
        _cut_walk_ml,
    )

    ml = _cut_walk_ml("Tablet_Parent.Parent", ["Tablet_Child.Child"])

    # Both attribution outputs are emitted for the parser.
    assert TRELLIS_RESIDUAL_TAG in ml
    assert TRELLIS_BOUNDARY_TAG in ml

    # The boundary set is embedded as checker text — the worker never supplies it.
    assert '["Tablet_Child.Child"]' in ml
    assert 'val root = "Tablet_Parent.Parent"' in ml

    # Identity is `Thm_Name.short` ALONE. It is already theory-qualified, while
    # `thm_node_theory_name` is SESSION-qualified, so concatenating them
    # double-qualifies and matches nothing (verified against Isabelle 2025-2).
    assert "Thm_Name.short (Proofterm.thm_node_name node)" in ml
    assert "thm_node_theory_name" not in ml

    # A boundary is RECORDED, not entered; everything else is descended into, so
    # nothing is silently skipped. The root's own self-box is never a boundary.
    assert "member (op =) boundaries id" in ml
    assert 'id <> root' in ml
    assert "Future.join (Proofterm.thm_node_body node)" in ml

    # `fold_body_thms` descends into child bodies BEFORE its callback, so it
    # cannot express a cut — it must not be used here.
    assert "fold_body_thms" not in ml


def test_pre_node_failure_is_a_transport_failure_not_a_proof_verdict() -> None:
    """REGRESSION: a reply with no node snapshot must not become `invalid_proof`.

    `use_theories` can fail during import/dependency resolution and never produce
    a snapshot. Isabelle reports that at the reply's TOP LEVEL (`kind`/`message`),
    not under `nodes[*].messages`. Reading diagnostics only from the node made
    such a reply indistinguishable from a completed run of a broken proof — no
    theorem, no statement, no dependencies, no errors — which the cert layer
    labelled `invalid_proof`, a mathematical verdict.

    That fabricated refutation was then compared against a clean warm certificate
    by the warm/cold cross-check, halting a live run on a closure "disagreement"
    that did not exist: the node's defining fact resolved oracle-free, and the
    five dependencies matched the warm cert exactly.
    """
    from trellis.atomic_actions.isabelle_observations import (
        STATUS_INTERNAL_ERROR,
        STATUS_INVALID_PROOF,
        local_closure_cert_envelope,
    )
    from trellis.checker.isabelle_session import CheckOutcome

    pre_node = CheckOutcome(
        ok=False,
        failed=1,
        finished=0,
        theory_name="Tablet_Node",
        node_name="",
        error_lines=['No such file: ".../Tablet_Missing.thy"'],
        pre_node_failure=True,
    )
    cert = local_closure_cert_envelope(pre_node)
    assert cert["status"] == STATUS_INTERNAL_ERROR
    # The real cause survives instead of being discarded.
    assert any("No such file" in e for e in cert["errors"])

    # A genuine proof failure — a snapshot WAS produced — still reports a verdict.
    real_failure = CheckOutcome(
        ok=False,
        failed=1,
        finished=3,
        theory_name="Tablet_Node",
        node_name="",
        error_lines=["Failed to finish proof"],
    )
    assert local_closure_cert_envelope(real_failure)["status"] == STATUS_INVALID_PROOF
