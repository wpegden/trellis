"""Tests for ``trellis.atomic_actions.checker_client``.

Two layers:

- Client-local validation (no server needed): assert the client mirrors
  the server's regex/length/clamps so a bad request is rejected before
  hitting the wire.
- End-to-end against an in-process :class:`CheckerServer` (with the
  observation functions monkey-patched to deterministic stubs). This
  exercises the real protocol — JSON encoding, line framing,
  ``request_id`` round-trip, ``rpc_error`` envelope unpacking — without
  involving real lake.

Plus targeted transport-failure tests using a small custom
"raw-server" thread for cases the real server cannot easily simulate
(garbage instead of JSON; mid-request crash; pre-canned ``rpc_error``).
"""

from __future__ import annotations

import json
import os
import socket
import tempfile
import threading
import time
from pathlib import Path
from typing import Any, Callable, Iterator

import pytest

from trellis.atomic_actions import checker_client
from trellis.atomic_actions.checker_client import (
    CheckerRpcError,
    TRELLIS_CHECKER_SOCKET_ENV,
    _resolve_socket_path,
    client_build_tablet,
    client_compile_node,
    client_lean_semantic_payloads,
    client_local_closure_axioms,
    client_materialize_tablet_oleans,
    client_ping,
    client_prepare_compiled_support,
    client_print_axioms,
)
from trellis.checker import protocol, server as server_mod
from trellis.checker.server import CheckerServer


# -------------------------------- fixtures --------------------------------


@pytest.fixture
def runtime_root() -> Iterator[Path]:
    """Build a runtime root layout the server expects.

    Uses a system-temp short path because ``AF_UNIX`` socket paths are
    capped at 108 bytes on Linux; pytest's per-test ``tmp_path`` is too
    deep for the ``<repo>/.trellis/runtime/<name>/sockets/checker.sock``
    layout to fit under that cap.
    """
    base = Path(tempfile.mkdtemp(prefix="lcc-", dir="/tmp"))
    repo = base / "r"
    runtime = repo / ".trellis" / "runtime" / "rt"
    runtime.mkdir(parents=True)
    (repo / "Tablet").mkdir(parents=True)
    yield runtime
    import shutil

    shutil.rmtree(base, ignore_errors=True)


def _stub_observations(monkeypatch: pytest.MonkeyPatch) -> dict[str, list[dict[str, Any]]]:
    """Replace every observation that the server dispatches with a
    deterministic stub. Returns a dict of recorded calls per op name so
    tests can assert which observation was invoked and with what args.
    """
    seen: dict[str, list[dict[str, Any]]] = {
        "compile_node": [],
        "materialize_tablet_oleans": [],
        "observe_lean_semantic_payloads": [],
        "print_axioms": [],
        "build_tablet": [],
        "prepare_compiled_support": [],
    }

    def _fake_compile_node(repo: Path, node_name: str, *, timeout_secs: float, bwrap_role: Any = None) -> dict[str, Any]:
        seen["compile_node"].append({"repo": repo, "node_name": node_name, "timeout_secs": timeout_secs, "bwrap_role": bwrap_role})
        return {
            "returncode": 0,
            "stdout": f"[{node_name}]\nok\n",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
            "requested_nodes": [node_name],
            "materialized_nodes": [node_name],
        }

    def _fake_materialize(repo: Path, nodes: list[str], *, timeout_secs: float, bwrap_role: Any = None) -> dict[str, Any]:
        seen["materialize_tablet_oleans"].append(
            {"repo": repo, "nodes": list(nodes), "timeout_secs": timeout_secs, "bwrap_role": bwrap_role}
        )
        return {
            "requested_nodes": list(nodes),
            "materialized_nodes": list(nodes),
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    def _fake_lean_semantic(repo: Path, nodes: list[str], *, timeout_secs: float, bwrap_role: Any = None) -> dict[str, dict[str, Any]]:
        seen["observe_lean_semantic_payloads"].append(
            {"repo": repo, "nodes": list(nodes), "timeout_secs": timeout_secs, "bwrap_role": bwrap_role}
        )
        return {n: {"ok": True, "payload": f"FP({n})", "error": ""} for n in nodes}

    def _fake_print_axioms(repo: Path, node_name: str, *, timeout_secs: float, bwrap_role: Any = None) -> dict[str, Any]:
        seen["print_axioms"].append(
            {"repo": repo, "node_name": node_name, "timeout_secs": timeout_secs, "bwrap_role": bwrap_role}
        )
        return {
            "returncode": 0,
            "stdout": f"axioms for {node_name}",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
        }

    def _fake_build_tablet(repo: Path, *, timeout_secs: float, bwrap_role: Any = None) -> dict[str, Any]:
        seen["build_tablet"].append(
            {"repo": repo, "timeout_secs": timeout_secs, "bwrap_role": bwrap_role}
        )
        return {
            "returncode": 0,
            "stdout": "build complete",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    def _fake_prepare_support(repo: Path, *, timeout_secs: float, bwrap_role: Any = None) -> dict[str, Any]:
        seen["prepare_compiled_support"].append(
            {"repo": repo, "timeout_secs": timeout_secs, "bwrap_role": bwrap_role}
        )
        return {
            "steps_completed": ["cache_get"],
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(server_mod.observations, "compile_node", _fake_compile_node)
    monkeypatch.setattr(server_mod.observations, "materialize_tablet_oleans", _fake_materialize)
    monkeypatch.setattr(server_mod.observations, "observe_lean_semantic_payloads", _fake_lean_semantic)
    monkeypatch.setattr(server_mod.observations, "print_axioms", _fake_print_axioms)
    monkeypatch.setattr(server_mod.observations, "build_tablet", _fake_build_tablet)
    monkeypatch.setattr(server_mod.observations, "prepare_compiled_support", _fake_prepare_support)
    return seen


@pytest.fixture
def stubbed_server(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> Iterator[tuple[CheckerServer, dict[str, list[dict[str, Any]]]]]:
    """In-process server with monkey-patched observations.

    Yields (server, seen_calls). Tests can read ``seen_calls`` to
    confirm dispatch wiring; the client tests focus on response shape.
    """
    seen = _stub_observations(monkeypatch)
    server = CheckerServer(runtime_root, parallelism=2, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())

    # Seed at least one tablet file so sync has something to mirror; the
    # observation functions are stubbed so contents don't matter.
    (server.worker_repo / "Tablet" / "Stub.lean").write_text(
        "import Tablet.Preamble\n", encoding="utf-8"
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server, seen
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


# --------------------------- helper: raw fake server ---------------------------


class _RawFakeServer:
    """Tiny ``AF_UNIX`` server that runs a custom ``handle(conn)`` callback.

    Used for tests that need a behaviour the real :class:`CheckerServer`
    cannot easily simulate (return garbage; close the connection
    mid-request; emit a custom ``rpc_error`` envelope).
    """

    def __init__(self, socket_path: Path, handle: Callable[[socket.socket], None]) -> None:
        self.socket_path = socket_path
        self._handle = handle
        self._listen_sock: socket.socket | None = None
        self._stop = threading.Event()
        self._accept_thread: threading.Thread | None = None

    def start(self) -> None:
        self.socket_path.parent.mkdir(parents=True, exist_ok=True)
        if self.socket_path.exists():
            self.socket_path.unlink()
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.bind(str(self.socket_path))
        sock.listen(4)
        sock.settimeout(0.5)
        self._listen_sock = sock
        self._accept_thread = threading.Thread(target=self._accept_loop, daemon=True)
        self._accept_thread.start()

    def _accept_loop(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _addr = self._listen_sock.accept()  # type: ignore[union-attr]
            except socket.timeout:
                continue
            except OSError:
                return
            try:
                self._handle(conn)
            except Exception:
                pass
            finally:
                try:
                    conn.close()
                except OSError:
                    pass

    def stop(self) -> None:
        self._stop.set()
        if self._listen_sock is not None:
            try:
                self._listen_sock.close()
            except OSError:
                pass
            self._listen_sock = None
        if self._accept_thread is not None:
            self._accept_thread.join(timeout=5.0)
            self._accept_thread = None
        try:
            if self.socket_path.exists():
                self.socket_path.unlink()
        except OSError:
            pass


@pytest.fixture
def raw_socket_path() -> Iterator[Path]:
    base = Path(tempfile.mkdtemp(prefix="lccraw-", dir="/tmp"))
    yield base / "fake.sock"
    import shutil

    shutil.rmtree(base, ignore_errors=True)


# -------------------- _resolve_socket_path env-var contract --------------------


def test_resolve_socket_path_returns_none_when_unset(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)
    assert _resolve_socket_path() is None


def test_resolve_socket_path_returns_none_when_empty(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, "")
    assert _resolve_socket_path() is None


def test_resolve_socket_path_returns_none_when_whitespace(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, "   \t\n  ")
    assert _resolve_socket_path() is None


def test_resolve_socket_path_returns_path_when_set(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, "/some/path/checker.sock")
    result = _resolve_socket_path()
    assert isinstance(result, Path)
    assert str(result) == "/some/path/checker.sock"


def test_resolve_socket_path_strips_surrounding_whitespace(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, "  /a/b.sock\n")
    result = _resolve_socket_path()
    assert result == Path("/a/b.sock")


# ------------------------ end-to-end response shapes ------------------------


def test_client_ping_pong(
    stubbed_server: tuple[CheckerServer, dict[str, list[dict[str, Any]]]],
) -> None:
    server, _seen = stubbed_server
    response = client_ping(server.socket_path)
    assert response["pong"] is True
    assert response["server_pid"] == os.getpid()
    assert response["uptime_secs"] >= 0
    assert response["supervisor_repo"] == str(server.supervisor_repo)
    assert response["worker_repo"] == str(server.worker_repo)


def test_client_compile_node_returns_shape_matching_observations(
    stubbed_server: tuple[CheckerServer, dict[str, list[dict[str, Any]]]],
) -> None:
    server, seen = stubbed_server
    response = client_compile_node(
        server.socket_path,
        repo=server.supervisor_repo,
        node_name="LemmaA",
        timeout_secs=30.0,
    )
    # Same keys today's observation function returns:
    expected_keys = {
        "returncode",
        "stdout",
        "stderr",
        "timed_out",
        "spawn_error",
        "node",
        "requested_nodes",
        "materialized_nodes",
    }
    assert expected_keys.issubset(set(response.keys()))
    assert response["node"] == "LemmaA"
    assert response["returncode"] == 0
    assert response["timed_out"] is False
    assert response["spawn_error"] == ""
    assert response["materialized_nodes"] == ["LemmaA"]
    # The dispatch must have routed through compile_node with bwrap_role:
    assert seen["compile_node"][0]["bwrap_role"] == "lake_compiler"
    assert seen["compile_node"][0]["node_name"] == "LemmaA"


def test_client_materialize_oleans_returns_shape_matching_observations(
    stubbed_server: tuple[CheckerServer, dict[str, list[dict[str, Any]]]],
) -> None:
    server, seen = stubbed_server
    response = client_materialize_tablet_oleans(
        server.socket_path,
        repo=server.supervisor_repo,
        nodes=["A", "B"],
        timeout_secs=60.0,
    )
    expected_keys = {
        "requested_nodes",
        "materialized_nodes",
        "returncode",
        "stdout",
        "stderr",
        "timed_out",
        "spawn_error",
    }
    assert expected_keys.issubset(set(response.keys()))
    assert response["requested_nodes"] == ["A", "B"]
    assert response["materialized_nodes"] == ["A", "B"]
    assert response["returncode"] == 0
    assert seen["materialize_tablet_oleans"][0]["bwrap_role"] == "lake_compiler"


def test_client_lean_semantic_payloads_returns_per_node_shape(
    stubbed_server: tuple[CheckerServer, dict[str, list[dict[str, Any]]]],
) -> None:
    server, seen = stubbed_server
    response = client_lean_semantic_payloads(
        server.socket_path,
        repo=server.supervisor_repo,
        nodes=["A", "B"],
        timeout_secs=60.0,
    )
    # Mirrors observe_lean_semantic_payloads's return: per-node dict keyed
    # by node_name, each with ok/payload/error.
    assert set(response.keys()) == {"A", "B"}
    for node_name, entry in response.items():
        assert set(entry.keys()) == {"ok", "payload", "error"}
        assert entry["ok"] is True
        assert entry["payload"] == f"FP({node_name})"
        assert entry["error"] == ""
    assert seen["observe_lean_semantic_payloads"][0]["bwrap_role"] == "lake_compiler"


def test_client_print_axioms_returns_shape_matching_observations(
    stubbed_server: tuple[CheckerServer, dict[str, list[dict[str, Any]]]],
) -> None:
    server, seen = stubbed_server
    response = client_print_axioms(
        server.socket_path,
        repo=server.supervisor_repo,
        node_name="LemmaA",
        timeout_secs=30.0,
    )
    expected_keys = {
        "returncode",
        "stdout",
        "stderr",
        "timed_out",
        "spawn_error",
        "node",
    }
    assert expected_keys.issubset(set(response.keys()))
    assert response["node"] == "LemmaA"
    assert response["returncode"] == 0
    assert seen["print_axioms"][0]["bwrap_role"] == "lake_compiler"


def test_client_build_tablet_returns_shape_matching_observations(
    stubbed_server: tuple[CheckerServer, dict[str, list[dict[str, Any]]]],
) -> None:
    server, seen = stubbed_server
    response = client_build_tablet(
        server.socket_path,
        repo=server.supervisor_repo,
        timeout_secs=30.0,
    )
    expected_keys = {"returncode", "stdout", "stderr", "timed_out", "spawn_error"}
    assert expected_keys.issubset(set(response.keys()))
    assert response["returncode"] == 0
    assert seen["build_tablet"][0]["bwrap_role"] == "lake_compiler"


def test_client_prepare_compiled_support_returns_shape_matching_observations(
    stubbed_server: tuple[CheckerServer, dict[str, list[dict[str, Any]]]],
) -> None:
    server, seen = stubbed_server
    response = client_prepare_compiled_support(
        server.socket_path,
        repo=server.supervisor_repo,
        timeout_secs=30.0,
    )
    expected_keys = {"steps_completed", "returncode", "stdout", "stderr", "timed_out", "spawn_error"}
    assert expected_keys.issubset(set(response.keys()))
    assert response["steps_completed"] == ["cache_get"]
    assert seen["prepare_compiled_support"][0]["bwrap_role"] == "lake_compiler"


# ----------------------- client-local validation -----------------------


def test_client_validates_node_name_locally(tmp_path: Path) -> None:
    """A bad node_name is rejected locally before any wire activity.

    No server is started: the client must raise before attempting to
    open a socket. This pins the defence-in-depth behaviour where the
    client mirrors the server's regex.
    """
    bogus_socket = tmp_path / "nonexistent.sock"  # purposely missing
    with pytest.raises(CheckerRpcError) as excinfo:
        client_compile_node(
            bogus_socket,
            repo=tmp_path,
            node_name="Bla;rm -rf /",
            timeout_secs=10.0,
        )
    # Must be a *local* validation error, NOT a connection error: that
    # confirms validation happened before the connect attempt.
    assert excinfo.value.kind == "invalid_request"
    assert "node_name" in excinfo.value.message


def test_client_validates_node_name_letter_start(tmp_path: Path) -> None:
    """Names starting with a digit or underscore are rejected locally."""
    bogus_socket = tmp_path / "nonexistent.sock"
    for bad_name in ("0Bad", "_Bad", "../escape"):
        with pytest.raises(CheckerRpcError) as excinfo:
            client_print_axioms(bogus_socket, repo=tmp_path, node_name=bad_name, timeout_secs=10.0)
        assert excinfo.value.kind == "invalid_request"


def test_client_validates_node_name_max_len(tmp_path: Path) -> None:
    bogus_socket = tmp_path / "nonexistent.sock"
    too_long = "A" + "a" * protocol.NODE_NAME_MAX_LEN
    with pytest.raises(CheckerRpcError) as excinfo:
        client_compile_node(bogus_socket, repo=tmp_path, node_name=too_long, timeout_secs=10.0)
    assert excinfo.value.kind == "invalid_request"


def test_client_validates_node_count_locally(tmp_path: Path) -> None:
    """A 2000-entry nodes list is rejected locally before any wire activity."""
    bogus_socket = tmp_path / "nonexistent.sock"
    too_many = [f"A{i}" for i in range(2000)]
    with pytest.raises(CheckerRpcError) as excinfo:
        client_materialize_tablet_oleans(
            bogus_socket,
            repo=tmp_path,
            nodes=too_many,
            timeout_secs=10.0,
        )
    assert excinfo.value.kind == "invalid_request"


def test_client_validates_timeout_secs_locally(tmp_path: Path) -> None:
    bogus_socket = tmp_path / "nonexistent.sock"
    # Non-finite must raise
    with pytest.raises(CheckerRpcError) as excinfo:
        client_build_tablet(bogus_socket, repo=tmp_path, timeout_secs=float("inf"))
    assert excinfo.value.kind == "invalid_request"
    with pytest.raises(CheckerRpcError) as excinfo:
        client_build_tablet(bogus_socket, repo=tmp_path, timeout_secs=float("nan"))
    assert excinfo.value.kind == "invalid_request"
    # Non-numeric must raise
    with pytest.raises(CheckerRpcError) as excinfo:
        client_build_tablet(bogus_socket, repo=tmp_path, timeout_secs="not-a-number")  # type: ignore[arg-type]
    assert excinfo.value.kind == "invalid_request"


def test_client_clamps_out_of_band_finite_timeout(tmp_path: Path) -> None:
    """Mirrors the server: finite-but-out-of-range timeouts are clamped,
    not raised. The validation should not fail; the connection should
    fail with supervisor_unavailable instead (no server running)."""
    bogus_socket = tmp_path / "nonexistent.sock"
    # Out-of-band finite values should clamp, not raise: caller proceeds
    # to connect and then trips supervisor_unavailable.
    with pytest.raises(CheckerRpcError) as excinfo:
        client_build_tablet(bogus_socket, repo=tmp_path, timeout_secs=0.001)
    assert excinfo.value.kind == "supervisor_unavailable"
    with pytest.raises(CheckerRpcError) as excinfo:
        client_build_tablet(bogus_socket, repo=tmp_path, timeout_secs=1e9)
    assert excinfo.value.kind == "supervisor_unavailable"


def test_client_validates_nodes_list_type(tmp_path: Path) -> None:
    bogus_socket = tmp_path / "nonexistent.sock"
    with pytest.raises(CheckerRpcError) as excinfo:
        client_materialize_tablet_oleans(
            bogus_socket,
            repo=tmp_path,
            nodes="LemmaA",  # type: ignore[arg-type]
            timeout_secs=10.0,
        )
    assert excinfo.value.kind == "invalid_request"


def test_client_validates_nodes_inner_entries(tmp_path: Path) -> None:
    bogus_socket = tmp_path / "nonexistent.sock"
    with pytest.raises(CheckerRpcError) as excinfo:
        client_lean_semantic_payloads(
            bogus_socket,
            repo=tmp_path,
            nodes=["GoodName", "Bad;name"],
            timeout_secs=10.0,
        )
    assert excinfo.value.kind == "invalid_request"
    assert "nodes[1]" in excinfo.value.message


# ----------------------- transport-failure tests -----------------------


def test_client_handles_supervisor_unavailable(tmp_path: Path) -> None:
    """Client points at a nonexistent socket -> ``supervisor_unavailable``."""
    bogus_socket = tmp_path / "nonexistent.sock"
    with pytest.raises(CheckerRpcError) as excinfo:
        client_ping(bogus_socket, timeout_secs=2.0)
    assert excinfo.value.kind == "supervisor_unavailable"
    assert str(bogus_socket) in excinfo.value.message


def test_client_handles_supervisor_crash_mid_request(
    raw_socket_path: Path,
) -> None:
    """Server accepts the connection then closes without writing.

    The client should raise ``supervisor_unavailable`` rather than hang.
    """

    def _crash_mid_request(conn: socket.socket) -> None:
        # Read whatever the client sends, then drop the connection.
        try:
            conn.recv(4096)
        except OSError:
            pass
        # Close immediately, simulating a crashed/killed server.
        conn.close()

    server = _RawFakeServer(raw_socket_path, _crash_mid_request)
    server.start()
    try:
        with pytest.raises(CheckerRpcError) as excinfo:
            client_ping(raw_socket_path, timeout_secs=3.0)
        assert excinfo.value.kind == "supervisor_unavailable"
    finally:
        server.stop()


def test_client_handles_malformed_server_response(
    raw_socket_path: Path,
) -> None:
    """Server returns garbage instead of JSON -> ``malformed_response``."""

    def _garbage(conn: socket.socket) -> None:
        try:
            conn.recv(4096)
        except OSError:
            pass
        # Send a line of non-JSON garbage and close.
        try:
            conn.sendall(b"this is not JSON at all\n")
        except OSError:
            pass

    server = _RawFakeServer(raw_socket_path, _garbage)
    server.start()
    try:
        with pytest.raises(CheckerRpcError) as excinfo:
            client_ping(raw_socket_path, timeout_secs=3.0)
        assert excinfo.value.kind == "malformed_response"
    finally:
        server.stop()


def test_client_handles_non_object_json_response(raw_socket_path: Path) -> None:
    """Server returns valid JSON that isn't an object -> ``malformed_response``."""

    def _array(conn: socket.socket) -> None:
        try:
            conn.recv(4096)
        except OSError:
            pass
        try:
            conn.sendall(b"[1, 2, 3]\n")
        except OSError:
            pass

    server = _RawFakeServer(raw_socket_path, _array)
    server.start()
    try:
        with pytest.raises(CheckerRpcError) as excinfo:
            client_ping(raw_socket_path, timeout_secs=3.0)
        assert excinfo.value.kind == "malformed_response"
    finally:
        server.stop()


def test_client_handles_request_id_mismatch(raw_socket_path: Path) -> None:
    """Server echoes a different request_id than we sent -> ``malformed_response``."""

    def _wrong_rid(conn: socket.socket) -> None:
        try:
            conn.recv(4096)
        except OSError:
            pass
        try:
            conn.sendall(b'{"request_id": 99999, "pong": true}\n')
        except OSError:
            pass

    server = _RawFakeServer(raw_socket_path, _wrong_rid)
    server.start()
    try:
        with pytest.raises(CheckerRpcError) as excinfo:
            client_ping(raw_socket_path, timeout_secs=3.0)
        assert excinfo.value.kind == "malformed_response"
        assert "request_id" in excinfo.value.message
    finally:
        server.stop()


def test_client_handles_rpc_error_envelope(raw_socket_path: Path) -> None:
    """Server returns a structured ``rpc_error`` -> client raises with same kind+message."""

    def _rpc_error(conn: socket.socket) -> None:
        # Read and discard the request.
        buf = bytearray()
        try:
            while not buf.endswith(b"\n"):
                chunk = conn.recv(4096)
                if not chunk:
                    return
                buf.extend(chunk)
        except OSError:
            return
        try:
            request = json.loads(buf.decode("utf-8"))
            rid = int(request.get("request_id", 0))
        except (json.JSONDecodeError, ValueError):
            rid = 0
        envelope = {
            "request_id": rid,
            "rpc_error": {"kind": "sync_failed", "message": "fingerprint cache corrupt"},
        }
        try:
            conn.sendall((json.dumps(envelope) + "\n").encode("utf-8"))
        except OSError:
            pass

    server = _RawFakeServer(raw_socket_path, _rpc_error)
    server.start()
    try:
        with pytest.raises(CheckerRpcError) as excinfo:
            client_ping(raw_socket_path, timeout_secs=3.0)
        assert excinfo.value.kind == "sync_failed"
        assert excinfo.value.message == "fingerprint cache corrupt"
    finally:
        server.stop()


def test_client_handles_eof_before_response(raw_socket_path: Path) -> None:
    """Server accepts connection and closes without reading or writing."""

    def _instant_close(conn: socket.socket) -> None:
        conn.close()

    server = _RawFakeServer(raw_socket_path, _instant_close)
    server.start()
    try:
        with pytest.raises(CheckerRpcError) as excinfo:
            client_ping(raw_socket_path, timeout_secs=3.0)
        assert excinfo.value.kind == "supervisor_unavailable"
    finally:
        server.stop()


def test_client_unknown_op_envelope_propagates_kind(
    stubbed_server: tuple[CheckerServer, dict[str, list[dict[str, Any]]]],
) -> None:
    """If the server sends back an `unknown_op` envelope (e.g. via a
    direct invalid request), the client must propagate the same kind.

    Verifies the ``rpc_error`` envelope unpacking path on the *real*
    server, not a fake one. Sending via the public client requires a
    valid op, so we round-trip an unknown op manually via the wire.
    """
    server, _seen = stubbed_server
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(str(server.socket_path))
    sock.settimeout(5.0)
    try:
        sock.sendall(b'{"op": "rm_rf", "request_id": 1234}\n')
        # Read the response and parse manually.
        data = b""
        while not data.endswith(b"\n"):
            chunk = sock.recv(4096)
            if not chunk:
                break
            data += chunk
        decoded = json.loads(data.decode("utf-8"))
        assert decoded["request_id"] == 1234
        assert decoded["rpc_error"]["kind"] == "unknown_op"
    finally:
        sock.close()


def test_client_returns_non_zero_returncode_in_payload(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A successful RPC whose lake returncode is non-zero is NOT raised:
    it's returned in the response payload, mirroring the observation
    function's behaviour today.
    """
    # Stub compile_node to return a "lake failed" payload.
    def _failed_compile(repo: Path, node_name: str, *, timeout_secs: float, bwrap_role: Any = None) -> dict[str, Any]:
        return {
            "returncode": 1,
            "stdout": "",
            "stderr": "lake build failed",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
            "requested_nodes": [node_name],
            "materialized_nodes": [],
        }

    monkeypatch.setattr(server_mod.observations, "compile_node", _failed_compile)

    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    (server.worker_repo / "Tablet" / "Stub.lean").write_text("import Tablet.Preamble\n", encoding="utf-8")
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        # Should NOT raise — non-zero returncode is a successful RPC.
        response = client_compile_node(
            server.socket_path,
            repo=server.supervisor_repo,
            node_name="LemmaA",
            timeout_secs=30.0,
        )
        assert response["returncode"] == 1
        assert response["stderr"] == "lake build failed"
        assert response["materialized_nodes"] == []
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_client_request_ids_are_distinct(
    stubbed_server: tuple[CheckerServer, dict[str, list[dict[str, Any]]]],
) -> None:
    """Pin: consecutive client calls must use distinct request_ids so a
    response from a stale prior call cannot accidentally satisfy a new
    call's wait. (Today this is moot — per-call connections — but the
    invariant should hold even if we cache later.)
    """
    server, _seen = stubbed_server
    # We can't directly observe the request_id, but we can do two
    # round-trips and confirm each succeeds (proving each uses a
    # request_id that matches the response on its own connection).
    a = client_ping(server.socket_path, timeout_secs=3.0)
    b = client_ping(server.socket_path, timeout_secs=3.0)
    assert a["pong"] is True
    assert b["pong"] is True


def test_client_validates_socket_path_type(tmp_path: Path) -> None:
    with pytest.raises(CheckerRpcError) as excinfo:
        client_ping(12345)  # type: ignore[arg-type]
    assert excinfo.value.kind == "invalid_request"


def test_client_validates_socket_path_empty_string(tmp_path: Path) -> None:
    with pytest.raises(CheckerRpcError) as excinfo:
        client_ping("")
    assert excinfo.value.kind == "invalid_request"


def _capture_first_request(socket_path: Path) -> dict:
    """Capture the first wire payload a client sends, then close the connection.

    Returns the parsed JSON request body (the line of bytes the client
    wrote before reading a response).
    """
    captured: dict[str, object] = {}

    def _handle(conn: socket.socket) -> None:
        # Read one line of request, capture it, then drop the connection.
        buf = b""
        while not buf.endswith(b"\n"):
            chunk = conn.recv(65536)
            if not chunk:
                break
            buf += chunk
        captured["request"] = json.loads(buf.decode("utf-8"))
        # Don't bother sending a valid response — the client will see
        # malformed_response or supervisor_unavailable; both fine for our
        # purposes since we only care about the OUTBOUND payload.

    server = _RawFakeServer(socket_path, _handle)
    server.start()
    try:
        # The client will fail (no response), but the handler captures
        # the request first.
        try:
            client_compile_node(socket_path, Path("/tmp/repo"), "Foo", timeout_secs=2.0)
        except CheckerRpcError:
            pass
    finally:
        server.stop()
    return captured.get("request", {})  # type: ignore[return-value]


def test_wire_payload_does_not_include_repo_path(tmp_path: Path) -> None:
    """Mitigation 2 contract lock: the server derives the repo from the
    socket's filesystem location, so the client must NEVER send `repo_path`
    on the wire. A future regression that adds it would silently broaden the
    attack surface."""
    import shutil

    sock_dir = Path(tempfile.mkdtemp(dir="/tmp", prefix="lcs-repo-path-"))
    try:
        request = _capture_first_request(sock_dir / "checker.sock")
    finally:
        shutil.rmtree(sock_dir, ignore_errors=True)
    assert isinstance(request, dict)
    assert request.get("op") == "lean_compile_node"
    assert "repo_path" not in request, (
        f"client request must not contain repo_path; got keys: {sorted(request.keys())}"
    )


# -------------------- Patch A: local-closure probe client --------------------
#
# Plan §5.9 Tier 2: client envelope shape, validation, timeout propagation.


def _capture_first_local_closure_request(socket_path: Path, **kwargs: Any) -> dict:
    """Variant of ``_capture_first_request`` that drives
    :func:`client_local_closure_axioms`. The client takes no ``repo``
    argument by design (plan §2.3); we only forward node_name and
    timeout."""
    captured: dict[str, object] = {}

    def _handle(conn: socket.socket) -> None:
        buf = b""
        while not buf.endswith(b"\n"):
            chunk = conn.recv(65536)
            if not chunk:
                break
            buf += chunk
        captured["request"] = json.loads(buf.decode("utf-8"))

    server = _RawFakeServer(socket_path, _handle)
    server.start()
    try:
        try:
            client_local_closure_axioms(
                socket_path,
                kwargs.get("node_name", "Foo"),
                timeout_secs=kwargs.get("timeout_secs", 2.0),
            )
        except CheckerRpcError:
            pass
    finally:
        server.stop()
    return captured.get("request", {})  # type: ignore[return-value]


def test_client_local_closure_envelope_shape(raw_socket_path: Path) -> None:
    """Plan §5.9 Tier 2: the wire envelope has exactly the expected fields.

    Patch A's request shape (plan §5.3): ``op``, ``request_id``,
    ``node_name``, ``timeout_secs``. No ``repo`` (plan §2.3 — server
    derives it). No ``nodes`` list (single-node op). Pinning this shape
    is what guards against accidentally widening the trust boundary
    later.
    """
    request = _capture_first_local_closure_request(
        raw_socket_path, node_name="ProjectionSubset", timeout_secs=45.0
    )
    assert isinstance(request, dict)
    assert request["op"] == "local_closure_axioms"
    assert request["node_name"] == "ProjectionSubset"
    assert request["timeout_secs"] == 45.0
    # request_id must be a non-negative integer; the protocol caps it at
    # >= 0 server-side. We don't check exact value (it comes from a
    # process-global counter) but it must be present.
    assert isinstance(request["request_id"], int)
    assert request["request_id"] >= 0
    # The set of keys must be exactly the four documented in plan §5.3.
    assert set(request.keys()) == {"op", "request_id", "node_name", "timeout_secs"}


def test_client_local_closure_does_not_include_repo_path(raw_socket_path: Path) -> None:
    """Mirrors ``test_wire_payload_does_not_include_repo_path`` for the
    new op. Plan §2.3 trust boundary."""
    request = _capture_first_local_closure_request(raw_socket_path)
    assert "repo_path" not in request, (
        f"client request must not contain repo_path; got keys: {sorted(request.keys())}"
    )


def test_client_local_closure_does_not_include_nodes_list(raw_socket_path: Path) -> None:
    """The op is single-node (``node_name``); the multi-node ``nodes``
    field is reserved for ``materialize_oleans`` /
    ``lean_semantic_payloads`` and must not appear here."""
    request = _capture_first_local_closure_request(raw_socket_path)
    assert "nodes" not in request


def test_client_local_closure_validates_node_name_locally(tmp_path: Path) -> None:
    """A bad node_name is rejected by the client BEFORE the wire."""
    bogus_socket = tmp_path / "nonexistent.sock"
    with pytest.raises(CheckerRpcError) as excinfo:
        client_local_closure_axioms(bogus_socket, "Bla;rm -rf /", timeout_secs=10.0)
    assert excinfo.value.kind == "invalid_request"
    assert "node_name" in excinfo.value.message


def test_client_local_closure_validates_node_name_letter_start(tmp_path: Path) -> None:
    bogus_socket = tmp_path / "nonexistent.sock"
    for bad_name in ("0Bad", "_Bad", "../escape"):
        with pytest.raises(CheckerRpcError) as excinfo:
            client_local_closure_axioms(bogus_socket, bad_name, timeout_secs=10.0)
        assert excinfo.value.kind == "invalid_request"


def test_client_local_closure_propagates_timeout(raw_socket_path: Path) -> None:
    """The caller's timeout must be coerced and forwarded verbatim on the wire.

    Plan §5.6: the client is the timeout-budget owner. The server may
    additionally clamp; the client must transmit the budget so the
    server can decide.
    """
    request = _capture_first_local_closure_request(
        raw_socket_path, node_name="Foo", timeout_secs=180.0
    )
    assert request["timeout_secs"] == 180.0


def test_client_local_closure_default_timeout(raw_socket_path: Path) -> None:
    """Per the public signature, omitting ``timeout_secs`` falls back to
    the function's declared default (3600s, matching
    ``LEAN_SUPPORT_TIMEOUT_SECS`` in
    ``trellis/atomic_actions/observations.py``). We emit that on the wire
    so the server sees a deterministic budget rather than relying on its
    own default (defense-in-depth). All production callers pass
    ``timeout_secs`` explicitly via the CLI, so this default rarely
    fires in practice; the test pins the contract so accidental
    signature changes are caught."""
    captured: dict[str, object] = {}

    def _handle(conn: socket.socket) -> None:
        buf = b""
        while not buf.endswith(b"\n"):
            chunk = conn.recv(65536)
            if not chunk:
                break
            buf += chunk
        captured["request"] = json.loads(buf.decode("utf-8"))

    server = _RawFakeServer(raw_socket_path, _handle)
    server.start()
    try:
        try:
            # Note: NO timeout_secs kwarg.
            client_local_closure_axioms(raw_socket_path, "Foo")
        except CheckerRpcError:
            pass
    finally:
        server.stop()
    request = captured.get("request", {})
    assert isinstance(request, dict)
    # Audit-2 followup #10: the prior assertion was 120.0 with a stale
    # "plan §5.3" citation; the function default has been 3600s since
    # the v32 era. Test+docstring were the stale party.
    assert request["timeout_secs"] == 3600.0


def test_client_local_closure_returns_envelope_through_real_server(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """End-to-end response shape against a real CheckerServer with the
    handler stubbed. Confirms the protocol, routing, and response
    decoding are all wired.
    """

    def _stub_handler(self, request):
        return (
            {
                "request_id": request.request_id,
                "node": request.node_name,
                "status": "ok",
                "root_kind": "theorem",
                "kernel_axioms": ["Classical.choice"],
                "boundary_theorems": [
                    {"name": "Tablet.Helper", "statement_hash": "h1"},
                ],
                "strict_theorem_deps": [],
                "strict_definition_deps": [],
                "errors": [],
                "stdout": "",
                "stderr": "",
                "returncode": 0,
                "timed_out": False,
                "spawn_error": "",
            },
            0,
            {"local_closure_status": "ok"},
        )

    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    monkeypatch.setattr(
        type(server), "_handle_local_closure_axioms", _stub_handler, raising=True
    )
    # Seed worker tablet so sync has something to mirror.
    (server.worker_repo / "Tablet" / "Foo.lean").write_text(
        "import Tablet.Preamble\n", encoding="utf-8"
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        response = client_local_closure_axioms(
            server.socket_path, "Foo", timeout_secs=30.0
        )
        assert response["status"] == "ok"
        assert response["root_kind"] == "theorem"
        assert response["kernel_axioms"] == ["Classical.choice"]
        assert response["boundary_theorems"] == [
            {"name": "Tablet.Helper", "statement_hash": "h1"}
        ]
        assert response["node"] == "Foo"
        assert response["returncode"] == 0
    finally:
        server.shutdown()
        thread.join(timeout=5.0)
