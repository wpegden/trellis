"""Tests for ``trellis.checker.isabelle_session`` (checker B2a).

Exercise the Isabelle server TCP protocol client against a FAKE
``servers.db`` + an in-process fake TCP server speaking the wire-verified
framing (``isabelle_install_notes.md`` Appendix A), without spawning a real
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
    oracles, deps, exists, shyps, stmt = _parse_cert_from_writeln(writeln, "triv")
    assert oracles == []
    assert deps == ["BitM_def", "One_nat_def", "add_Suc_right"]
    assert exists is True
    assert shyps == []
    assert stmt == ""


def test_cert_parser_sorry_surfaces_skip_proof() -> None:
    """A ``sorry`` proof taints the theorem with the ``skip_proof`` oracle
    (Isabelle's ``Pure.skip_proof``); the parser must surface it so the
    B2c-gate can reject it."""
    writeln = [
        "theorem viaSorry: 1 + 1 = 2",
        "oracles:\n    skip_proof",
    ]
    oracles, deps, exists, _shyps, _stmt = _parse_cert_from_writeln(writeln, "viaSorry")
    assert oracles == ["skip_proof"]
    assert exists is True


def test_cert_parser_missing_theorem_when_name_absent() -> None:
    """When the named theorem's ``theorem`` line is absent (oops/elision),
    ``theorem_exists`` is False even if some unrelated theorem line appears."""
    writeln = ["theorem other: True", "oracles:"]
    oracles, deps, exists, _shyps, _stmt = _parse_cert_from_writeln(writeln, "triv")
    assert exists is False


def test_cert_parser_handles_inline_oracle_names() -> None:
    writeln = ["oracles: skip_proof", "dependencies: 1\n    refl"]
    oracles, deps, _exists, _shyps, _stmt = _parse_cert_from_writeln(writeln, None)
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
    sess = IsabelleSession(name=name, start_timeout_secs=600.0)
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
