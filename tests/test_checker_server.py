"""Tests for ``trellis.checker.server``.

Exercise the protocol validator, socket-hygiene properties (mode, peer-uid
check, in-flight cap, line-size cap), and a ping/pong round-trip end-to-end
against a temp runtime root. The integration test that drives a real lake
invocation lives in /tmp (see the PR's report); this file uses monkey-patched
observation functions so unit tests run without lake.
"""

from __future__ import annotations

import json
import os
import socket
import stat
import struct
import threading
import time
from pathlib import Path
from typing import Any

import pytest

from trellis.checker import protocol, server as server_mod
from trellis.checker.protocol import (
    MAX_LINE_BYTES,
    NODE_NAME_MAX_LEN,
    ProtocolError,
    parse_line,
    rpc_error_envelope,
    validate_request,
)
from trellis.checker.server import CheckerServer


# ----------------------------- protocol layer -----------------------------


def test_node_name_regex_matches_filespec_letter_start() -> None:
    assert protocol.NODE_NAME_REGEX.fullmatch("LemmaA") is not None
    assert protocol.NODE_NAME_REGEX.fullmatch("TwoBites_LemmaA") is not None
    # Must START with a letter (per FILESPEC.md identifier convention).
    assert protocol.NODE_NAME_REGEX.fullmatch("0Bad") is None
    assert protocol.NODE_NAME_REGEX.fullmatch("_Bad") is None
    # Must not contain dots / slashes / nul.
    assert protocol.NODE_NAME_REGEX.fullmatch("Foo.Bar") is None
    assert protocol.NODE_NAME_REGEX.fullmatch("../escape") is None
    assert protocol.NODE_NAME_REGEX.fullmatch("a\x00b") is None
    assert protocol.NODE_NAME_REGEX.fullmatch("") is None


def test_validate_request_rejects_oversized_node_name() -> None:
    too_long = "A" + "a" * NODE_NAME_MAX_LEN
    with pytest.raises(ProtocolError) as excinfo:
        validate_request({"op": "verify_node", "request_id": 1, "node_name": too_long})
    assert excinfo.value.kind == "malformed_request"


def test_validate_request_rejects_oversized_nodes_list() -> None:
    nodes = [f"A{i}" for i in range(protocol.MAX_NODES_PER_REQUEST + 1)]
    with pytest.raises(ProtocolError) as excinfo:
        validate_request({"op": "materialize_oleans", "request_id": 1, "nodes": nodes})
    assert excinfo.value.kind == "malformed_request"


def test_validate_request_rejects_unknown_op() -> None:
    with pytest.raises(ProtocolError) as excinfo:
        validate_request({"op": "rm_rf", "request_id": 1})
    assert excinfo.value.kind == "unknown_op"


def test_validate_request_rejects_repo_path_field() -> None:
    with pytest.raises(ProtocolError) as excinfo:
        validate_request(
            {"op": "verify_node", "request_id": 1, "node_name": "A", "repo_path": "/etc"}
        )
    assert excinfo.value.kind == "malformed_request"


def test_validate_request_clamps_timeout_into_band() -> None:
    too_low = validate_request(
        {"op": "lean_build_tablet", "request_id": 1, "timeout_secs": 0.01}
    )
    assert too_low.timeout_secs == protocol.TIMEOUT_SECS_MIN
    too_high = validate_request(
        {"op": "lean_build_tablet", "request_id": 2, "timeout_secs": 1e9}
    )
    assert too_high.timeout_secs == protocol.TIMEOUT_SECS_MAX


def test_validate_request_rejects_non_finite_timeout() -> None:
    with pytest.raises(ProtocolError):
        validate_request(
            {"op": "lean_build_tablet", "request_id": 1, "timeout_secs": float("inf")}
        )


def test_parse_line_rejects_oversized_line() -> None:
    huge = b"a" * (MAX_LINE_BYTES + 1) + b"\n"
    with pytest.raises(ProtocolError) as excinfo:
        parse_line(huge)
    assert excinfo.value.kind == "malformed_request"


def test_parse_line_rejects_non_object_json() -> None:
    with pytest.raises(ProtocolError):
        parse_line(b"[1,2,3]\n")


def test_rpc_error_envelope_shape() -> None:
    env = rpc_error_envelope(7, "internal_error", "boom")
    assert env == {
        "request_id": 7,
        "rpc_error": {"kind": "internal_error", "message": "boom"},
    }


# --------------------------- path resolution ---------------------------


def test_resolve_worker_repo_inner_form(tmp_path: Path) -> None:
    repo = tmp_path / "myrepo"
    runtime = repo / ".trellis" / "runtime" / "main"
    runtime.mkdir(parents=True)
    assert server_mod._resolve_worker_repo_for_runtime(runtime) == repo.resolve()


def test_resolve_worker_repo_outer_form(tmp_path: Path) -> None:
    repo = tmp_path / "myrepo"
    (repo / ".trellis").mkdir(parents=True)
    runtime = tmp_path / "myrepo-runtime"
    runtime.mkdir()
    assert server_mod._resolve_worker_repo_for_runtime(runtime) == repo.resolve()


def test_resolve_worker_repo_outer_form_missing_sibling_repo_raises(tmp_path: Path) -> None:
    runtime = tmp_path / "ghost-runtime"
    runtime.mkdir()
    with pytest.raises(ValueError, match="sibling repo"):
        server_mod._resolve_worker_repo_for_runtime(runtime)


def test_resolve_worker_repo_unrecognized_layout_raises(tmp_path: Path) -> None:
    runtime = tmp_path / "not_a_runtime"
    runtime.mkdir()
    with pytest.raises(ValueError, match="path layout unexpected"):
        server_mod._resolve_worker_repo_for_runtime(runtime)


# --------------------------- server fixtures ---------------------------


@pytest.fixture
def runtime_root(tmp_path_factory) -> Path:
    """Build a runtime root layout the server expects: ``<repo>/.trellis/runtime/<name>``.

    Uses a system-temp short path because ``AF_UNIX`` socket paths are
    capped at 108 bytes on Linux; pytest's per-test ``tmp_path`` is too
    deep for the ``<repo>/.trellis/runtime/<name>/sockets/checker.sock``
    layout to fit under that cap.
    """
    import tempfile

    base = Path(tempfile.mkdtemp(prefix="lcs-", dir="/tmp"))
    repo = base / "r"
    runtime = repo / ".trellis" / "runtime" / "rt"
    runtime.mkdir(parents=True)
    (repo / "Tablet").mkdir(parents=True)
    yield runtime
    import shutil

    shutil.rmtree(base, ignore_errors=True)


@pytest.fixture
def started_server(runtime_root: Path):
    server = CheckerServer(runtime_root, parallelism=2, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def _connect(server: CheckerServer) -> socket.socket:
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(str(server.socket_path))
    sock.settimeout(5.0)
    return sock


def _round_trip(server: CheckerServer, payload: dict[str, Any]) -> dict[str, Any]:
    sock = _connect(server)
    try:
        sock.sendall((json.dumps(payload) + "\n").encode("utf-8"))
        data = b""
        while not data.endswith(b"\n"):
            chunk = sock.recv(4096)
            if not chunk:
                break
            data += chunk
        return json.loads(data.decode("utf-8"))
    finally:
        sock.close()


# ----------------------------- socket hygiene -----------------------------


def test_socket_file_mode_is_0o600(started_server: CheckerServer) -> None:
    # Phase 2 of the bwrap-only migration (SANDBOX_BWRAP_ONLY_MIGRATION_PLAN_2026-06-03.md)
    # tightened the socket mode 0o660 → 0o600; group-rw is no longer
    # required because Phase 4 (still pending) will collapse the
    # burst-user separation onto the supervisor's own uid.
    st = os.stat(started_server.socket_path)
    assert stat.S_IMODE(st.st_mode) == 0o600


def test_socket_is_filesystem_not_abstract(started_server: CheckerServer) -> None:
    # Filesystem socket: the socket node exists on disk.
    assert started_server.socket_path.exists()
    assert stat.S_ISSOCK(started_server.socket_path.stat().st_mode)


def test_peer_uid_check_rejects_mismatched_uid(
    runtime_root: Path,
) -> None:
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    # Force a uid that the test process cannot match.
    server.set_expected_peer_uid(os.geteuid() + 999_999)
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    sock: socket.socket | None = None
    try:
        sock = _connect(server)
        try:
            sock.sendall(b'{"op": "ping", "request_id": 1}\n')
            # Server closes the connection without writing anything.
            sock.settimeout(2.0)
            try:
                data = sock.recv(4096)
            except (ConnectionResetError, socket.timeout):
                data = b""
        except BrokenPipeError:
            # Equivalent rejection outcome: the server accepted and closed
            # before the client wrote its request bytes.
            data = b""
        assert data == b""
    finally:
        if sock is not None:
            sock.close()
        server.shutdown()
        thread.join(timeout=5.0)


# ----------------------------- Phase 2 (bwrap-only) token gate -----------------------------
# SANDBOX_BWRAP_ONLY_MIGRATION_PLAN_2026-06-03.md §3. Once any token is
# registered (file or in-process), the server's per-request gate
# requires a matching ``auth_token`` field on the request envelope.
# When no token is registered (dormant Phase-2 state), the gate is
# inactive and the legacy UID check is the only line of defence —
# this is the property that lets Phase 2 source ship without
# restarting the live checker.


def test_request_without_token_is_rejected(runtime_root: Path) -> None:
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    server.register_burst_token("hunter2")
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        response = _round_trip(server, {"op": "ping", "request_id": 1})
        assert "rpc_error" in response, response
        assert response["rpc_error"]["kind"] == "auth_required", response
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_request_with_unknown_token_is_rejected(runtime_root: Path) -> None:
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    server.register_burst_token("good-token")
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        response = _round_trip(
            server,
            {"op": "ping", "request_id": 1, "auth_token": "bad-token"},
        )
        assert "rpc_error" in response, response
        assert response["rpc_error"]["kind"] == "auth_required", response
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_distinct_tokens_isolate_bursts(runtime_root: Path) -> None:
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    server.register_burst_token("token-a")
    server.register_burst_token("token-b")
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        ok_a = _round_trip(
            server, {"op": "ping", "request_id": 1, "auth_token": "token-a"}
        )
        assert ok_a.get("pong") is True, ok_a
        ok_b = _round_trip(
            server, {"op": "ping", "request_id": 2, "auth_token": "token-b"}
        )
        assert ok_b.get("pong") is True, ok_b
        # Revoke token-a; subsequent requests with token-a fail, token-b
        # still works — distinct bursts genuinely isolated.
        server.revoke_burst_token("token-a")
        rejected = _round_trip(
            server, {"op": "ping", "request_id": 3, "auth_token": "token-a"}
        )
        assert "rpc_error" in rejected, rejected
        assert rejected["rpc_error"]["kind"] == "auth_required", rejected
        still_ok = _round_trip(
            server, {"op": "ping", "request_id": 4, "auth_token": "token-b"}
        )
        assert still_ok.get("pong") is True, still_ok
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_dormant_token_state_admits_unauthenticated_requests(
    runtime_root: Path,
) -> None:
    """When no tokens are registered, the server falls back to the
    legacy UID gate and accepts requests without an ``auth_token`` field.

    This is the load-bearing dormant-Phase-2 property: source can ship
    BEFORE the checker restart because the live checker (still on old
    code) ignores the token, and the new code (when restarted before
    Phase 4) ALSO ignores the token until something registers one.
    Workers sending an extra ``auth_token`` field in their request
    envelope must not cause an auth failure here either — the field is
    benign payload, not a "token gate active" signal.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    # No register_burst_token call → dormant state.
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        no_token = _round_trip(server, {"op": "ping", "request_id": 1})
        assert no_token.get("pong") is True, no_token
        with_token = _round_trip(
            server,
            {"op": "ping", "request_id": 2, "auth_token": "anything"},
        )
        assert with_token.get("pong") is True, with_token
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_concurrent_connection_cap_blocks_excess(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Reduce cap to 2 so the test doesn't have to chew up many sockets.
    monkeypatch.setattr(server_mod, "MAX_INFLIGHT_CONNECTIONS", 2)
    server = CheckerServer(runtime_root, parallelism=2, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())

    # Patch the request log out of the way and stub the dispatcher to block
    # on an event so we can hold connections open without a real lake.
    block = threading.Event()
    finished = threading.Event()

    def _slow_dispatch(req):
        block.wait(timeout=10.0)
        finished.set()
        return {"request_id": req.request_id, "stub": True}, None, {}

    monkeypatch.setattr(server, "_dispatch_op", _slow_dispatch)
    monkeypatch.setattr(server, "_assert_path_containment", lambda req: None)

    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        held: list[socket.socket] = []
        for _ in range(2):
            s = _connect(server)
            s.sendall(b'{"op": "lean_build_tablet", "request_id": 0}\n')
            held.append(s)
        # Wait briefly so the in-flight slots are taken.
        time.sleep(0.2)
        # Third connection: server accepts then immediately closes (cap hit).
        rejected = _connect(server)
        rejected.sendall(b'{"op": "ping", "request_id": 99}\n')
        rejected.settimeout(2.0)
        try:
            data = rejected.recv(4096)
        except (ConnectionResetError, socket.timeout):
            data = b""
        assert data == b""
        rejected.close()

        # Release blocked dispatchers so cleanup is clean.
        block.set()
        for s in held:
            try:
                s.recv(4096)
            except OSError:
                pass
            s.close()
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_oversize_line_returns_malformed_error(started_server: CheckerServer) -> None:
    sock = _connect(server := started_server)
    try:
        # Send 17 MiB (> 64 KiB line cap) without a newline; server should
        # detect line-cap overrun and respond with malformed_request.
        sock.sendall(b"a" * (server_mod.MAX_LINE_BYTES + 1024))
        sock.shutdown(socket.SHUT_WR)
        data = b""
        sock.settimeout(5.0)
        while True:
            try:
                chunk = sock.recv(4096)
            except OSError:
                break
            if not chunk:
                break
            data += chunk
        if data:
            decoded = json.loads(data.splitlines()[0].decode("utf-8"))
            assert decoded.get("rpc_error", {}).get("kind") == "malformed_request"
    finally:
        sock.close()


# -------------------------- end-to-end ping/dispatch --------------------------


def test_ping_round_trip(started_server: CheckerServer) -> None:
    response = _round_trip(started_server, {"op": "ping", "request_id": 42})
    assert response["request_id"] == 42
    assert response["pong"] is True
    assert response["server_pid"] == os.getpid()
    assert response["uptime_secs"] >= 0


def test_unknown_op_returns_clean_error_envelope(started_server: CheckerServer) -> None:
    response = _round_trip(started_server, {"op": "rm_rf", "request_id": 7})
    assert response["request_id"] == 7
    assert response["rpc_error"]["kind"] == "unknown_op"


def test_malformed_json_returns_clean_error_envelope(started_server: CheckerServer) -> None:
    sock = _connect(started_server)
    try:
        sock.sendall(b"{not json\n")
        data = b""
        sock.settimeout(5.0)
        while not data.endswith(b"\n"):
            chunk = sock.recv(4096)
            if not chunk:
                break
            data += chunk
        decoded = json.loads(data.decode("utf-8"))
        assert decoded["rpc_error"]["kind"] == "malformed_request"
    finally:
        sock.close()


def test_repo_path_field_rejected_in_round_trip(started_server: CheckerServer) -> None:
    response = _round_trip(
        started_server,
        {"op": "verify_node", "request_id": 1, "node_name": "A", "repo_path": "/etc"},
    )
    assert response["rpc_error"]["kind"] == "malformed_request"


def test_dispatch_calls_observation_with_lake_compiler_role(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Pin the wiring: every supervisor-side observation call must pass
    ``bwrap_role="lake_compiler"`` so threat-model mitigation 1 is active."""
    server = CheckerServer(runtime_root, parallelism=2, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    seen_kwargs: list[dict[str, Any]] = []

    def _fake_compile_node(repo, node_name, *, timeout_secs, bwrap_role=None):
        seen_kwargs.append({"node": node_name, "bwrap_role": bwrap_role})
        return {
            "returncode": 0,
            "stdout": f"[{node_name}]\n",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
            "requested_nodes": [node_name],
            "materialized_nodes": [node_name],
        }

    monkeypatch.setattr(server_mod.observations, "compile_node", _fake_compile_node)

    # Seed a worker tablet file so sync has something to mirror.
    (server.worker_repo / "Tablet" / "Stub.lean").write_text(
        "import Tablet.Preamble\n", encoding="utf-8"
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        response = _round_trip(
            server, {"op": "verify_node", "request_id": 11, "node_name": "Stub"}
        )
        assert response["request_id"] == 11
        assert response["returncode"] == 0
        assert seen_kwargs == [{"node": "Stub", "bwrap_role": "lake_compiler"}]
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_compile_cache_skips_lake_when_node_is_known_current(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Pin the compile-cache short-circuit: after a successful lake call
    that exited 0, the same lean_compile_node request with no sync diff
    must not re-invoke lake — the synthetic payload is returned and
    ``compile_cache_hit: True`` is logged.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    call_count = {"n": 0}

    def _fake_compile_node(repo, node_name, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        # Materialize the olean on disk so the post-call mtime check passes.
        olean = (
            repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / f"{node_name}.olean"
        )
        olean.parent.mkdir(parents=True, exist_ok=True)
        olean.write_bytes(b"fake-olean")
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
            "requested_nodes": [node_name],
            "materialized_nodes": [node_name],
        }

    monkeypatch.setattr(server_mod.observations, "compile_node", _fake_compile_node)

    # Seed worker tablet so sync is non-failing on first call.
    (server.worker_repo / "Tablet" / "Stub.lean").write_text(
        "import Tablet.Preamble\n", encoding="utf-8"
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        # First call: cache empty, falls through to lake.
        r1 = _round_trip(
            server, {"op": "lean_compile_node", "request_id": 1, "node_name": "Stub"}
        )
        # Second call with the same source state: must hit cache.
        r2 = _round_trip(
            server, {"op": "lean_compile_node", "request_id": 2, "node_name": "Stub"}
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)

    assert r1["returncode"] == 0
    assert r2["returncode"] == 0
    assert call_count["n"] == 1, "lake must be invoked exactly once across the two calls"

    log_path = runtime_root / "checker-state" / "server.log"
    records = [
        json.loads(line)
        for line in log_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    compile_records = [r for r in records if r["op"] == "lean_compile_node"]
    assert len(compile_records) == 2
    assert compile_records[0].get("compile_cache_hit") is False
    assert compile_records[1].get("compile_cache_hit") is True


def test_compile_cache_preserves_sorry_warning_on_synthetic_compile_hit(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A lean_compile_node cache hit must preserve Lean's sorry-warning fact.

    Rust uses compile stdout/stderr to decide whether an otherwise compiling
    node is still open because a dependency contains ``sorry``. A synthetic
    cache hit may skip lake, but it must not erase that fact.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    call_count = {"n": 0}

    def _fake_compile_node(repo, node_name, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        olean = (
            repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / f"{node_name}.olean"
        )
        olean.parent.mkdir(parents=True, exist_ok=True)
        olean.write_bytes(b"fake-olean")
        return {
            "returncode": 0,
            "stdout": "warning: Tablet/Helper.lean:3:8: declaration uses 'sorry'\n",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
            "requested_nodes": [node_name],
            "materialized_nodes": [node_name],
        }

    monkeypatch.setattr(server_mod.observations, "compile_node", _fake_compile_node)
    (server.worker_repo / "Tablet" / "Stub.lean").write_text(
        "import Tablet.Preamble\nimport Tablet.Helper\n", encoding="utf-8"
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        r1 = _round_trip(
            server, {"op": "lean_compile_node", "request_id": 1, "node_name": "Stub"}
        )
        r2 = _round_trip(
            server, {"op": "lean_compile_node", "request_id": 2, "node_name": "Stub"}
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)

    assert r1["returncode"] == 0
    assert r2["returncode"] == 0
    assert call_count["n"] == 1
    cached_output = f"{r2.get('stdout', '')}\n{r2.get('stderr', '')}".lower()
    assert "warning" in cached_output
    assert "sorry" in cached_output


def test_compile_cache_orphan_deletion_keeps_unrelated_nodes_cached_e2e(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """End-to-end production-like test: spin up a real server, do real
    socket RPCs, exercise the sync path. After warming the cache for
    Foo and Bar, simulate a worker that deletes its orphan Foo source
    file. The next sync detects the removal. The cache must evict
    only Foo; the next compile_node for Bar must short-circuit
    (no fake-lake invocation, ``compile_cache_hit: true`` logged).

    This is the test that actually exercises the bug we hit on
    cycle 26: a single orphan delete should NOT force a full re-warm.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    lake_calls: list[str] = []

    def _fake_compile_node(repo, node_name, *, timeout_secs, bwrap_role=None):
        lake_calls.append(node_name)
        olean = (
            repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / f"{node_name}.olean"
        )
        olean.parent.mkdir(parents=True, exist_ok=True)
        olean.write_bytes(b"fake-olean-" + node_name.encode())
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
            "requested_nodes": [node_name],
            "materialized_nodes": [node_name],
        }

    monkeypatch.setattr(server_mod.observations, "compile_node", _fake_compile_node)

    # Worker tablet has two unrelated files (Foo orphan, Bar unrelated).
    worker_tablet = server.worker_repo / "Tablet"
    worker_tablet.mkdir(parents=True, exist_ok=True)
    (worker_tablet / "Foo.lean").write_text(
        "import Tablet.Preamble\n", encoding="utf-8"
    )
    (worker_tablet / "Bar.lean").write_text(
        "import Tablet.Preamble\n", encoding="utf-8"
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        # Warm cache: each first call is a miss + real (fake) lake invocation.
        r1 = _round_trip(
            server, {"op": "lean_compile_node", "request_id": 1, "node_name": "Foo"}
        )
        r2 = _round_trip(
            server, {"op": "lean_compile_node", "request_id": 2, "node_name": "Bar"}
        )
        assert r1["returncode"] == 0 and r2["returncode"] == 0
        assert lake_calls == ["Foo", "Bar"]
        # Both nodes are now in the known-current set.
        assert "Foo" in server._oleans_known_current
        assert "Bar" in server._oleans_known_current

        # Worker deletes Foo (the orphan). Bar's source is unchanged.
        (worker_tablet / "Foo.lean").unlink()

        # Next call (any node) triggers sync, which removes Foo from
        # the supervisor's tablet too, surfacing it in sync_result.
        # With the new node-targeted invalidation, only Foo gets evicted.
        # Bar stays cached → next compile_node(Bar) is a cache hit (no
        # additional lake call).
        r3 = _round_trip(
            server, {"op": "lean_compile_node", "request_id": 3, "node_name": "Bar"}
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)

    assert r3["returncode"] == 0
    assert lake_calls == ["Foo", "Bar"], (
        f"expected exactly 2 lake invocations across 3 calls, "
        f"but got {lake_calls!r} — node-targeted invalidation is broken"
    )
    assert "Foo" not in server._oleans_known_current
    assert "Bar" in server._oleans_known_current

    log_path = runtime_root / "checker-state" / "server.log"
    records = [
        json.loads(line)
        for line in log_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    bar_records = [r for r in records if r.get("node") == "Bar" and r["op"] == "lean_compile_node"]
    assert len(bar_records) == 2
    assert bar_records[0].get("compile_cache_hit") is False, (
        "first Bar call should be a miss (warmup)"
    )
    assert bar_records[1].get("compile_cache_hit") is True, (
        "second Bar call should hit cache (Foo deletion is not Bar's concern)"
    )
    foo_records = [r for r in records if r.get("node") == "Foo"]
    assert any(r.get("sync_changed_files", 0) >= 1 for r in records[-3:]), (
        "the third call's sync should report Foo's removal"
    )


def test_compile_cache_modification_invalidates_consumer_e2e(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """End-to-end: when source X is modified and Y imports X, both
    must be evicted. Y's next compile call must NOT cache-hit.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    lake_calls: list[str] = []

    def _fake_compile_node(repo, node_name, *, timeout_secs, bwrap_role=None):
        lake_calls.append(node_name)
        olean = (
            repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / f"{node_name}.olean"
        )
        olean.parent.mkdir(parents=True, exist_ok=True)
        olean.write_bytes(b"fake-olean-" + node_name.encode())
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
            "requested_nodes": [node_name],
            "materialized_nodes": [node_name],
        }

    monkeypatch.setattr(server_mod.observations, "compile_node", _fake_compile_node)

    worker_tablet = server.worker_repo / "Tablet"
    worker_tablet.mkdir(parents=True, exist_ok=True)
    (worker_tablet / "Foo.lean").write_text(
        "import Tablet.Preamble\n", encoding="utf-8"
    )
    # Bar imports Foo: Foo's source change invalidates Bar's olean.
    (worker_tablet / "Bar.lean").write_text(
        "import Tablet.Preamble\nimport Tablet.Foo\n", encoding="utf-8"
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        _round_trip(
            server, {"op": "lean_compile_node", "request_id": 1, "node_name": "Foo"}
        )
        _round_trip(
            server, {"op": "lean_compile_node", "request_id": 2, "node_name": "Bar"}
        )
        assert lake_calls == ["Foo", "Bar"]

        # Worker modifies Foo's source.
        (worker_tablet / "Foo.lean").write_text(
            "import Tablet.Preamble\n-- mutated\n", encoding="utf-8"
        )
        # Next call to Bar must NOT hit cache (Foo changed → Bar stale).
        _round_trip(
            server, {"op": "lean_compile_node", "request_id": 3, "node_name": "Bar"}
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)

    assert lake_calls == ["Foo", "Bar", "Bar"], (
        f"expected Bar to be re-compiled after Foo's source changed, "
        f"got {lake_calls!r}"
    )
    assert "Foo" not in server._oleans_known_current
    # Bar got recompiled in call 3 → re-added to set.
    assert "Bar" in server._oleans_known_current


def test_compile_cache_invalidated_by_sync_changes(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """After lake succeeds and the cache records the node as current, a
    subsequent sync that surfaces a worker-side change MUST invalidate
    the cache; the next call falls through to lake again.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    call_count = {"n": 0}

    def _fake_compile_node(repo, node_name, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        olean = (
            repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / f"{node_name}.olean"
        )
        olean.parent.mkdir(parents=True, exist_ok=True)
        olean.write_bytes(b"fake-olean")
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
            "requested_nodes": [node_name],
            "materialized_nodes": [node_name],
        }

    monkeypatch.setattr(server_mod.observations, "compile_node", _fake_compile_node)
    (server.worker_repo / "Tablet" / "Stub.lean").write_text(
        "import Tablet.Preamble\n", encoding="utf-8"
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        _round_trip(
            server, {"op": "lean_compile_node", "request_id": 1, "node_name": "Stub"}
        )
        # Worker edits the source — sync will see this as a changed file.
        (server.worker_repo / "Tablet" / "Stub.lean").write_text(
            "import Tablet.Preamble\n-- edited\n", encoding="utf-8"
        )
        _round_trip(
            server, {"op": "lean_compile_node", "request_id": 2, "node_name": "Stub"}
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)

    assert call_count["n"] == 2, "sync change must force the second call back through lake"


def test_request_log_appends_one_record_per_request(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())

    monkeypatch.setattr(
        server_mod.observations,
        "build_tablet",
        lambda repo, *, timeout_secs, bwrap_role=None: {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        _round_trip(server, {"op": "lean_build_tablet", "request_id": 100})
        time.sleep(0.05)
    finally:
        server.shutdown()
        thread.join(timeout=5.0)

    log_path = runtime_root / "checker-state" / "server.log"
    assert log_path.is_file()
    lines = [
        json.loads(line)
        for line in log_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    assert any(rec["op"] == "lean_build_tablet" and rec["request_id"] == 100 for rec in lines)


def test_server_rejects_runtime_root_with_unexpected_layout(tmp_path: Path) -> None:
    bad = tmp_path / "not-runtime"
    bad.mkdir()
    with pytest.raises(ValueError):
        CheckerServer(bad)


def test_singleton_lock_blocks_second_server_for_same_runtime_root(
    runtime_root: Path,
) -> None:
    """Audit Fix 2: a second CheckerServer pointed at the same runtime
    must fail to ``start()`` so it cannot stomp the live instance's
    socket. The error must surface the existing PID for ops debugging.
    """
    first = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    first.set_expected_peer_uid(os.geteuid())
    first.start()
    try:
        second = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
        second.set_expected_peer_uid(os.geteuid())
        with pytest.raises(server_mod.SingletonError) as excinfo:
            second.start()
        assert excinfo.value.existing_pid == os.getpid()
        # Live socket must still be intact — second instance refused to start
        # before unlinking the socket.
        assert first.socket_path.exists()
    finally:
        first.shutdown()


def test_singleton_lock_releases_after_shutdown(runtime_root: Path) -> None:
    """After the first server shuts down cleanly, a fresh server can
    acquire the lock. Pins the lock-release path in shutdown()."""
    first = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    first.set_expected_peer_uid(os.geteuid())
    first.start()
    first.shutdown()

    # Second server should now be able to start without raising.
    second = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    second.set_expected_peer_uid(os.geteuid())
    second.start()
    try:
        assert second.socket_path.exists()
    finally:
        second.shutdown()


# --------------------------------------------------------------------------
# lean_semantic_payloads: per-node sidecar cache
# --------------------------------------------------------------------------


def _supervisor_olean_path(server: CheckerServer, node_name: str) -> Path:
    """Mirror ``observations._tablet_olean_path``. The cache-key path
    walks the supervisor's olean tree, not the worker's, because lake
    invocations run in the supervisor sandbox."""
    return (
        server.supervisor_repo
        / ".lake"
        / "build"
        / "lib"
        / "lean"
        / "Tablet"
        / f"{node_name}.olean"
    )


def _seed_supervisor_olean(server: CheckerServer, node_name: str, content: bytes) -> None:
    """Drop a stub ``.olean`` blob at the supervisor's expected path.
    Cache-key derivation only sha256's the bytes, so a plain blob is
    enough; we never invoke the lean toolchain in these tests."""
    p = _supervisor_olean_path(server, node_name)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(content)


def _seed_worker_for_semantic_cache(server: CheckerServer) -> None:
    """Stage a tiny Tablet chain in the worker repo + a fingerprint
    script + lean-toolchain pin so the cache can compute keys.

    The supervisor-side ``Tablet/*.lean`` tree is hydrated automatically
    by the server's pre-dispatch ``sync_tablet_dir`` call, but compiled
    artifacts (``.olean``) and the lake manifest are not on that sync
    surface — so we seed them directly on the supervisor repo here. The
    cache-key derivation rejects (returns ``None``) when an expected
    olean is absent, so without these stubs every request would fall
    through the cache as ``cache_unkeyed`` and the round-trip
    assertions would never reach the load/store paths.
    """
    tablet = server.worker_repo / "Tablet"
    tablet.mkdir(parents=True, exist_ok=True)
    (tablet / "Dep.lean").write_text("-- dep stub\n", encoding="utf-8")
    (tablet / "Leaf.lean").write_text(
        "import Tablet.Dep\n-- leaf stub\n", encoding="utf-8"
    )
    # The supervisor repo is what the dispatcher reads for hashing the
    # toolchain pin and lake manifest. The repo dir is created on first
    # sync; pre-create it here so we can drop the toolchain + manifest
    # files plus the olean stubs into place.
    super_repo = server.supervisor_repo
    super_repo.mkdir(parents=True, exist_ok=True)
    (super_repo / "lean-toolchain").write_text(
        "leanprover/lean4:v4.99.0\n", encoding="utf-8"
    )
    (super_repo / "lake-manifest.json").write_text(
        '{"version": 7, "packages": []}\n', encoding="utf-8"
    )
    _seed_supervisor_olean(server, "Dep", b"olean-dep-v0")
    _seed_supervisor_olean(server, "Leaf", b"olean-leaf-v0")
    # The fingerprint script is read off the source tree. It already
    # exists in the worktree; nothing to do here.


def test_lean_semantic_payloads_cache_hit_skips_observation(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """End-to-end smoke confirmation: a second request for the same node
    must return the cached payload WITHOUT calling the observation
    function. Hits the load → return-early branch through the public
    socket round-trip."""
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    _seed_worker_for_semantic_cache(server)

    call_count = {"n": 0}

    def _fake_observe(repo, node_names, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        return {
            name: {"ok": True, "payload": f"FP\t{name}\tdigest", "error": ""}
            for name in node_names
        }

    monkeypatch.setattr(
        server_mod.observations, "observe_lean_semantic_payloads", _fake_observe
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        first = _round_trip(
            server,
            {
                "op": "lean_semantic_payloads",
                "request_id": 1,
                "nodes": ["Leaf"],
            },
        )
        assert first["nodes"]["Leaf"]["ok"] is True
        assert first["nodes"]["Leaf"]["payload"] == "FP\tLeaf\tdigest"
        assert call_count["n"] == 1

        # Second request: same inputs, must hit the cache.
        second = _round_trip(
            server,
            {
                "op": "lean_semantic_payloads",
                "request_id": 2,
                "nodes": ["Leaf"],
            },
        )
        assert second["nodes"]["Leaf"]["ok"] is True
        assert second["nodes"]["Leaf"]["payload"] == "FP\tLeaf\tdigest"
        # The observation function was NOT invoked the second time —
        # this is the load-bearing assertion of the smoke test.
        assert call_count["n"] == 1
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_lean_semantic_payloads_cache_miss_after_dep_edit(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Editing a transitive Tablet-import dep MUST invalidate the cache:
    the cache key is content-addressed off the dep's sha, so any change
    forces a recomputation."""
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    _seed_worker_for_semantic_cache(server)

    payloads = {"v": "v1"}
    call_count = {"n": 0}

    def _fake_observe(repo, node_names, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        return {
            name: {
                "ok": True,
                "payload": f"FP\t{name}\t{payloads['v']}",
                "error": "",
            }
            for name in node_names
        }

    monkeypatch.setattr(
        server_mod.observations, "observe_lean_semantic_payloads", _fake_observe
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        first = _round_trip(
            server,
            {
                "op": "lean_semantic_payloads",
                "request_id": 1,
                "nodes": ["Leaf"],
            },
        )
        assert first["nodes"]["Leaf"]["payload"] == "FP\tLeaf\tv1"
        assert call_count["n"] == 1

        # Mutate the dep contents, then bump the payload value the
        # observation will return so we can tell whether the cache
        # served the stale or the fresh value.
        (server.worker_repo / "Tablet" / "Dep.lean").write_text(
            "-- dep MODIFIED\n", encoding="utf-8"
        )
        payloads["v"] = "v2"

        second = _round_trip(
            server,
            {
                "op": "lean_semantic_payloads",
                "request_id": 2,
                "nodes": ["Leaf"],
            },
        )
        # Cache key changed -> miss -> fresh observation -> v2.
        assert second["nodes"]["Leaf"]["payload"] == "FP\tLeaf\tv2"
        assert call_count["n"] == 2
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_lean_semantic_payloads_partial_miss_in_batch(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Hitting some nodes from cache and missing others in the same
    batch: only the misses should reach the observation function."""
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    _seed_worker_for_semantic_cache(server)

    # Add a second leaf (independent, so its cache entry is independent).
    (server.worker_repo / "Tablet" / "Leaf2.lean").write_text(
        "import Tablet.Dep\n-- second leaf\n", encoding="utf-8"
    )
    # Seed the supervisor-side olean stub so cache-key derivation
    # treats Leaf2 as a real miss (not ``cache_unkeyed``) — the test
    # exercises the cached vs. uncached split, which only fires when
    # both branches reach the load step.
    _seed_supervisor_olean(server, "Leaf2", b"olean-leaf2-v0")

    miss_calls: list[list[str]] = []

    def _fake_observe(repo, node_names, *, timeout_secs, bwrap_role=None):
        miss_calls.append(list(node_names))
        return {
            name: {"ok": True, "payload": f"FP\t{name}\tdigest", "error": ""}
            for name in node_names
        }

    monkeypatch.setattr(
        server_mod.observations, "observe_lean_semantic_payloads", _fake_observe
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        # Warm cache for Leaf only.
        _round_trip(
            server,
            {
                "op": "lean_semantic_payloads",
                "request_id": 1,
                "nodes": ["Leaf"],
            },
        )
        assert miss_calls == [["Leaf"]]

        # Batch request both. Server should only invoke the observation
        # for Leaf2 (the cold one); Leaf must come from the sidecar.
        second = _round_trip(
            server,
            {
                "op": "lean_semantic_payloads",
                "request_id": 2,
                "nodes": ["Leaf", "Leaf2"],
            },
        )
        assert second["nodes"]["Leaf"]["payload"] == "FP\tLeaf\tdigest"
        assert second["nodes"]["Leaf2"]["payload"] == "FP\tLeaf2\tdigest"
        # Two observation calls total: the first warm-up + this miss-only one.
        assert miss_calls == [["Leaf"], ["Leaf2"]]
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_lean_semantic_payloads_cache_hit_when_unrelated_node_changes(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Editing a node OUTSIDE the closure of the requested node MUST NOT
    invalidate the cache — the closure walk only includes transitive
    Tablet imports, so changing an unrelated node has no effect on the
    semantic payload.

    Setup: two leaves that don't import each other (NodeA, NodeB), each
    only importing the shared Dep stub. Warm both caches, then mutate
    NodeA's source. A subsequent request for NodeB must hit the cache
    (mock counter unchanged); a subsequent request for NodeA must miss
    (mock counter incremented).
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    _seed_worker_for_semantic_cache(server)

    # Replace the default Leaf with two independent leaves (NodeA, NodeB).
    # Both import only Tablet.Dep — no edge between A and B.
    (server.worker_repo / "Tablet" / "Leaf.lean").unlink()
    (server.worker_repo / "Tablet" / "NodeA.lean").write_text(
        "import Tablet.Dep\n-- node A original\n", encoding="utf-8"
    )
    (server.worker_repo / "Tablet" / "NodeB.lean").write_text(
        "import Tablet.Dep\n-- node B original\n", encoding="utf-8"
    )
    # Seed each leaf's olean stub so cache-key derivation succeeds.
    _seed_supervisor_olean(server, "NodeA", b"olean-nodea-v0")
    _seed_supervisor_olean(server, "NodeB", b"olean-nodeb-v0")

    call_count = {"n": 0}

    def _fake_observe(repo, node_names, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        return {
            name: {"ok": True, "payload": f"FP\t{name}\tdigest", "error": ""}
            for name in node_names
        }

    monkeypatch.setattr(
        server_mod.observations, "observe_lean_semantic_payloads", _fake_observe
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        # Warm both caches.
        _round_trip(
            server,
            {"op": "lean_semantic_payloads", "request_id": 1, "nodes": ["NodeA"]},
        )
        _round_trip(
            server,
            {"op": "lean_semantic_payloads", "request_id": 2, "nodes": ["NodeB"]},
        )
        assert call_count["n"] == 2  # one observe call per warm-up

        # Mutate NodeA. NodeB's closure (={NodeB, Dep}) is untouched, so
        # its cache key is unchanged.
        (server.worker_repo / "Tablet" / "NodeA.lean").write_text(
            "import Tablet.Dep\n-- node A MUTATED\n", encoding="utf-8"
        )

        # Request NodeB — closure-key unchanged, must hit the cache.
        nb = _round_trip(
            server,
            {"op": "lean_semantic_payloads", "request_id": 3, "nodes": ["NodeB"]},
        )
        assert nb["nodes"]["NodeB"]["payload"] == "FP\tNodeB\tdigest"
        # The load-bearing assertion: the observation was NOT re-invoked
        # for NodeB just because NodeA's source changed.
        assert call_count["n"] == 2

        # Sanity check: requesting NodeA itself DOES miss after mutation.
        na = _round_trip(
            server,
            {"op": "lean_semantic_payloads", "request_id": 4, "nodes": ["NodeA"]},
        )
        assert na["nodes"]["NodeA"]["payload"] == "FP\tNodeA\tdigest"
        assert call_count["n"] == 3
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


# --------------------------------------------------------------------------
# print_axioms: per-node sidecar cache (Fix 1)
# --------------------------------------------------------------------------


def test_print_axioms_cache_hit_skips_lake_on_second_call(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix 1: a successful print_axioms call must persist to the sidecar
    cache; the second call with no source/olean changes must short-circuit
    without invoking the observation. Mirrors the semantic-payload cache
    smoke test.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    _seed_worker_for_semantic_cache(server)

    call_count = {"n": 0}

    def _fake_print_axioms(repo, node_name, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        return {
            "returncode": 0,
            "stdout": f"'{node_name}' depends on axioms: [propext]\n",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
        }

    monkeypatch.setattr(server_mod.observations, "print_axioms", _fake_print_axioms)
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        first = _round_trip(
            server, {"op": "print_axioms", "request_id": 1, "node_name": "Leaf"}
        )
        assert first["returncode"] == 0
        assert "propext" in first["stdout"]
        assert call_count["n"] == 1

        # Second call: identical inputs → cache hit, no observation call.
        second = _round_trip(
            server, {"op": "print_axioms", "request_id": 2, "node_name": "Leaf"}
        )
        assert second["returncode"] == 0
        assert "propext" in second["stdout"]
        assert call_count["n"] == 1, (
            "second print_axioms call must hit the cache; observation re-invoked"
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)

    log_path = runtime_root / "checker-state" / "server.log"
    records = [
        json.loads(line)
        for line in log_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    pa_records = [r for r in records if r.get("op") == "print_axioms"]
    assert len(pa_records) == 2
    assert pa_records[0].get("print_axioms_cache_hit") is False
    assert pa_records[1].get("print_axioms_cache_hit") is True


def test_print_axioms_cache_miss_after_olean_change(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix 1: changing the supervisor's olean for the requested node
    must invalidate the cache key. The next call must re-invoke the
    observation rather than serve a stale axioms list.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    _seed_worker_for_semantic_cache(server)

    payloads = {"v": "v1"}
    call_count = {"n": 0}

    def _fake_print_axioms(repo, node_name, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        return {
            "returncode": 0,
            "stdout": f"'{node_name}' depends on axioms: [{payloads['v']}]\n",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
        }

    monkeypatch.setattr(server_mod.observations, "print_axioms", _fake_print_axioms)
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        first = _round_trip(
            server, {"op": "print_axioms", "request_id": 1, "node_name": "Leaf"}
        )
        assert first["stdout"] == "'Leaf' depends on axioms: [v1]\n"
        assert call_count["n"] == 1

        # Rewrite the supervisor olean — the cache key embeds the olean
        # sha, so the new request must miss.
        _seed_supervisor_olean(server, "Leaf", b"olean-leaf-v1-MUTATED")
        payloads["v"] = "v2"

        second = _round_trip(
            server, {"op": "print_axioms", "request_id": 2, "node_name": "Leaf"}
        )
        assert second["stdout"] == "'Leaf' depends on axioms: [v2]\n"
        assert call_count["n"] == 2
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_print_axioms_failure_not_cached(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix 1: cache only successes. A failure (non-zero rc, timeout,
    spawn_error) must NOT be persisted; the next call must reach the
    observation again so transient errors don't lock in.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    _seed_worker_for_semantic_cache(server)

    call_count = {"n": 0}

    def _fake_print_axioms(repo, node_name, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        return {
            "returncode": 1,
            "stdout": "",
            "stderr": f"'{node_name}': error: failed to elaborate\n",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
        }

    monkeypatch.setattr(server_mod.observations, "print_axioms", _fake_print_axioms)
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        first = _round_trip(
            server, {"op": "print_axioms", "request_id": 1, "node_name": "Leaf"}
        )
        assert first["returncode"] == 1
        assert call_count["n"] == 1

        # Same inputs as before, but the previous failure must not be
        # cached → observation MUST be called again.
        second = _round_trip(
            server, {"op": "print_axioms", "request_id": 2, "node_name": "Leaf"}
        )
        assert second["returncode"] == 1
        assert call_count["n"] == 2, (
            "failed print_axioms calls must not be cached; observation skipped"
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_print_axioms_cache_unkeyed_when_olean_missing(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix 1: when the supervisor olean for the requested node is absent,
    the cache key derivation returns None and the request passes through
    to lake every time. No persistence happens.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    # Seed worker with a tablet but DO NOT seed the supervisor olean —
    # cache-key derivation must short-circuit to None.
    tablet = server.worker_repo / "Tablet"
    tablet.mkdir(parents=True, exist_ok=True)
    (tablet / "Solo.lean").write_text("-- solo stub\n", encoding="utf-8")
    super_repo = server.supervisor_repo
    super_repo.mkdir(parents=True, exist_ok=True)
    (super_repo / "lean-toolchain").write_text(
        "leanprover/lean4:v4.99.0\n", encoding="utf-8"
    )
    (super_repo / "lake-manifest.json").write_text(
        '{"version": 7, "packages": []}\n', encoding="utf-8"
    )
    # Note: no _seed_supervisor_olean call → key derivation returns None.

    call_count = {"n": 0}

    def _fake_print_axioms(repo, node_name, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        return {
            "returncode": 0,
            "stdout": f"'{node_name}' depends on axioms: [propext]\n",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
        }

    monkeypatch.setattr(server_mod.observations, "print_axioms", _fake_print_axioms)
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        for rid in (1, 2, 3):
            _round_trip(
                server,
                {"op": "print_axioms", "request_id": rid, "node_name": "Solo"},
            )
        # Every call must reach lake — no key, no cache.
        assert call_count["n"] == 3, (
            f"expected 3 observation calls when cache is unkeyed, got {call_count['n']}"
        )
        # The on-disk sidecar dir must remain absent (or empty) — nothing
        # was persisted.
        cache_dir = server.print_axioms_cache_dir
        if cache_dir.exists():
            entries = [p for p in cache_dir.iterdir() if p.suffix == ".json"]
            assert entries == [], (
                f"unkeyed requests must not persist sidecars, found {entries!r}"
            )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


# --------------------------------------------------------------------------
# materialize_oleans: subset (Fix 2 server-side)
# --------------------------------------------------------------------------


def _seed_supervisor_lean(server: CheckerServer, node: str, body: str = "") -> None:
    """Seed a Tablet/<node>.lean source on the SUPERVISOR repo so the
    materialize_oleans dispatch path's _compute_compile_cache_subset
    stat-check can compare olean mtime to source mtime."""
    tablet = server.supervisor_repo / "Tablet"
    tablet.mkdir(parents=True, exist_ok=True)
    (tablet / f"{node}.lean").write_text(body or "-- stub\n", encoding="utf-8")


def test_materialize_oleans_partial_cache_hit_passes_only_uncached_to_lake(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix 2 (server-side): when a subset of requested nodes is already
    known-current, materialize_oleans must pass ONLY the uncached nodes
    to the observation function. The merged response must list every
    requested node in materialized_nodes.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())

    # Seed worker tablet and supervisor olean stubs for all three nodes.
    worker_tablet = server.worker_repo / "Tablet"
    worker_tablet.mkdir(parents=True, exist_ok=True)
    for node in ("A", "B", "C"):
        (worker_tablet / f"{node}.lean").write_text(
            "import Tablet.Preamble\n", encoding="utf-8"
        )

    seen_node_lists: list[list[str]] = []

    def _fake_materialize(repo, node_names, *, timeout_secs, bwrap_role=None):
        seen_node_lists.append(list(node_names))
        # Materialize each requested node so the stat-walk picks them up.
        olean_dir = repo / ".lake" / "build" / "lib" / "lean" / "Tablet"
        olean_dir.mkdir(parents=True, exist_ok=True)
        for n in node_names:
            (olean_dir / f"{n}.olean").write_bytes(b"fake-olean-" + n.encode())
        return {
            "requested_nodes": list(node_names),
            "materialized_nodes": list(node_names),
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(
        server_mod.observations, "materialize_tablet_oleans", _fake_materialize
    )

    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        # Warm cache for A and B by issuing materialize_oleans for them.
        first = _round_trip(
            server,
            {"op": "materialize_oleans", "request_id": 1, "nodes": ["A", "B"]},
        )
        assert first["returncode"] == 0
        assert sorted(first["materialized_nodes"]) == ["A", "B"]
        assert seen_node_lists == [["A", "B"]]
        # Both nodes are now in the known-current set.
        assert {"A", "B"}.issubset(server._oleans_known_current)

        # Request all three: A and B should be cache hits, only C goes
        # to lake. Bump C's source ahead of supervisor sync so it's a
        # new node lake hasn't seen.
        second = _round_trip(
            server,
            {"op": "materialize_oleans", "request_id": 2, "nodes": ["A", "B", "C"]},
        )
        assert second["returncode"] == 0
        # Merged response must list every requested node (lake's closure
        # plus the cached subset). Set equality, not order: the merged
        # list interleaves lake-output order and cached entries.
        assert sorted(second["materialized_nodes"]) == ["A", "B", "C"]
        # Lake's second invocation must see ONLY the uncached node.
        assert seen_node_lists == [["A", "B"], ["C"]], (
            f"expected lake's second call to see only ['C'] (the uncached "
            f"subset), got {seen_node_lists!r}"
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)

    log_path = runtime_root / "checker-state" / "server.log"
    records = [
        json.loads(line)
        for line in log_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    mat_records = [r for r in records if r.get("op") == "materialize_oleans"]
    assert len(mat_records) == 2
    assert mat_records[0].get("compile_cache_hit") is False
    assert mat_records[0].get("compile_cache_nodes") == 0
    assert mat_records[1].get("compile_cache_hit") is True
    assert mat_records[1].get("compile_cache_nodes") == 2


def test_materialize_oleans_full_cache_hit_skips_lake_entirely(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fix 2 (server-side): when every requested node is cached, the
    observation MUST NOT be invoked at all (today's behaviour). Pin
    this so the new partial-cache-hit logic doesn't accidentally
    regress the full-hit fast path.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())

    worker_tablet = server.worker_repo / "Tablet"
    worker_tablet.mkdir(parents=True, exist_ok=True)
    (worker_tablet / "A.lean").write_text(
        "import Tablet.Preamble\n", encoding="utf-8"
    )

    call_count = {"n": 0}

    def _fake_materialize(repo, node_names, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        olean_dir = repo / ".lake" / "build" / "lib" / "lean" / "Tablet"
        olean_dir.mkdir(parents=True, exist_ok=True)
        for n in node_names:
            (olean_dir / f"{n}.olean").write_bytes(b"fake-olean-" + n.encode())
        return {
            "requested_nodes": list(node_names),
            "materialized_nodes": list(node_names),
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(
        server_mod.observations, "materialize_tablet_oleans", _fake_materialize
    )

    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        # Warm cache.
        _round_trip(
            server, {"op": "materialize_oleans", "request_id": 1, "nodes": ["A"]}
        )
        assert call_count["n"] == 1

        # Repeat same request: full cache hit, lake must NOT be called.
        second = _round_trip(
            server, {"op": "materialize_oleans", "request_id": 2, "nodes": ["A"]}
        )
        assert second["returncode"] == 0
        assert second["materialized_nodes"] == ["A"]
        assert call_count["n"] == 1, (
            "full cache hit must short-circuit; observation re-invoked"
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)

    log_path = runtime_root / "checker-state" / "server.log"
    records = [
        json.loads(line)
        for line in log_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    mat_records = [r for r in records if r.get("op") == "materialize_oleans"]
    assert len(mat_records) == 2
    assert mat_records[1].get("compile_cache_hit") is True
    assert mat_records[1].get("compile_cache_nodes") == 1


# --------------------------------------------------------------------------
# Optimization #1: prepare_compiled_support manifest-sha cache
# --------------------------------------------------------------------------


def test_prepare_compiled_support_short_circuits_on_matching_manifest(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Optimization #1: after a successful prepare, a subsequent call with
    the same ``lake-manifest.json`` sha must skip the lake invocation and
    return a synthetic success. ``lake exe cache get`` is idempotent for
    a given manifest revision, so the on-disk cache is already populated.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    # Seed supervisor's lake-manifest so the cache key derives non-empty.
    server.supervisor_repo.mkdir(parents=True, exist_ok=True)
    (server.supervisor_repo / "lake-manifest.json").write_text(
        '{"version": 7, "packages": []}\n', encoding="utf-8"
    )
    call_count = {"n": 0}

    def _fake_prepare(repo, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        return {
            "steps_completed": ["cache_get"],
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(server_mod.observations, "prepare_compiled_support", _fake_prepare)
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        first = _round_trip(
            server, {"op": "prepare_compiled_support", "request_id": 1}
        )
        assert first["returncode"] == 0
        assert call_count["n"] == 1
        # Subsequent call with unchanged manifest must short-circuit.
        for rid in (2, 3):
            response = _round_trip(
                server, {"op": "prepare_compiled_support", "request_id": rid}
            )
            assert response["returncode"] == 0
        assert call_count["n"] == 1, (
            "second/third prepare must hit cache; observation re-invoked"
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)

    log_path = runtime_root / "checker-state" / "server.log"
    records = [
        json.loads(line)
        for line in log_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    pp_records = [r for r in records if r.get("op") == "prepare_compiled_support"]
    assert len(pp_records) == 3
    assert pp_records[0].get("prepare_cache_hit") is False
    assert pp_records[1].get("prepare_cache_hit") is True
    assert pp_records[2].get("prepare_cache_hit") is True


def test_prepare_compiled_support_re_fires_when_manifest_changes(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Optimization #1: after the manifest changes (e.g. ``lake update``
    bumped the mathlib rev), the next prepare must reach the observation
    again rather than serve the stale on-disk cache.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    server.supervisor_repo.mkdir(parents=True, exist_ok=True)
    (server.supervisor_repo / "lake-manifest.json").write_text(
        '{"version": 7, "packages": []}\n', encoding="utf-8"
    )
    call_count = {"n": 0}

    def _fake_prepare(repo, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        return {
            "steps_completed": ["cache_get"],
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(server_mod.observations, "prepare_compiled_support", _fake_prepare)
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        _round_trip(server, {"op": "prepare_compiled_support", "request_id": 1})
        assert call_count["n"] == 1
        # Mutate the manifest; the cache must invalidate.
        (server.supervisor_repo / "lake-manifest.json").write_text(
            '{"version": 7, "packages": [{"name": "mathlib"}]}\n', encoding="utf-8"
        )
        _round_trip(server, {"op": "prepare_compiled_support", "request_id": 2})
        assert call_count["n"] == 2, (
            "manifest change must force the second prepare to reach lake"
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_prepare_compiled_support_failure_not_cached(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Optimization #1: a failed prepare (non-zero rc, timeout, spawn
    error) must not seed the cache; the next call must reach the
    observation again so a transient flake doesn't lock in a permanent
    skip.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    server.supervisor_repo.mkdir(parents=True, exist_ok=True)
    (server.supervisor_repo / "lake-manifest.json").write_text(
        '{"version": 7, "packages": []}\n', encoding="utf-8"
    )
    call_count = {"n": 0}

    def _fake_prepare(repo, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        return {
            "steps_completed": [],
            "returncode": 2,
            "stdout": "",
            "stderr": "lake fetch failed\n",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(server_mod.observations, "prepare_compiled_support", _fake_prepare)
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        for rid in (1, 2):
            r = _round_trip(
                server, {"op": "prepare_compiled_support", "request_id": rid}
            )
            assert r["returncode"] == 2
        assert call_count["n"] == 2, "failures must not be cached"
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


# --------------------------------------------------------------------------
# Optimization #2: cache-hit fast path skips workspace_lock
# --------------------------------------------------------------------------


def test_cache_hit_fast_path_does_not_block_on_inflight_lake(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Optimization #2: a thread that holds ``_workspace_lock`` (lake
    in-flight) must NOT block other threads' cache-hit responses. With
    the split sync_lock / lake_lock design, cache-hit responses only
    take ``_sync_lock`` (briefly during sync) and then return without
    contending on the lake lock.
    """
    server = CheckerServer(runtime_root, parallelism=4, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())

    # Seed two nodes: Foo (will be cache-hit-able after first warmup)
    # and Bar (will be the thread blocking lake_lock).
    worker_tablet = server.worker_repo / "Tablet"
    worker_tablet.mkdir(parents=True, exist_ok=True)
    (worker_tablet / "Foo.lean").write_text("import Tablet.Preamble\n", encoding="utf-8")
    (worker_tablet / "Bar.lean").write_text("import Tablet.Preamble\n", encoding="utf-8")

    def _fake_compile_node(repo, node_name, *, timeout_secs, bwrap_role=None):
        olean = repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / f"{node_name}.olean"
        olean.parent.mkdir(parents=True, exist_ok=True)
        olean.write_bytes(b"fake-olean-" + node_name.encode())
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
            "node": node_name,
            "requested_nodes": [node_name],
            "materialized_nodes": [node_name],
        }

    monkeypatch.setattr(server_mod.observations, "compile_node", _fake_compile_node)

    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        # Warm the cache for Foo via the public socket so a subsequent
        # request can be a cache hit.
        _round_trip(
            server, {"op": "lean_compile_node", "request_id": 1, "node_name": "Foo"}
        )
        assert "Foo" in server._oleans_known_current

        # Manually grab the lake lock to simulate an in-flight long lake call.
        server._workspace_lock.acquire()
        try:
            # While the lake lock is held by THIS thread, fire a cache-hit
            # request for Foo from another thread. With the optimization,
            # that request should NOT block on workspace_lock — it only
            # needs sync_lock (which is free). We bound the wait so a
            # regression manifests as a test failure rather than a hang.
            response_holder: dict = {}
            def _do_cache_hit():
                try:
                    response_holder["r"] = _round_trip(
                        server,
                        {
                            "op": "lean_compile_node",
                            "request_id": 2,
                            "node_name": "Foo",
                        },
                    )
                except Exception as exc:
                    response_holder["err"] = exc

            t = threading.Thread(target=_do_cache_hit, daemon=True)
            t.start()
            t.join(timeout=5.0)
            assert not t.is_alive(), (
                "cache-hit request blocked on workspace_lock while lake "
                "was held by the test thread — concurrency optimization broke"
            )
            assert "r" in response_holder, response_holder.get("err")
            assert response_holder["r"]["returncode"] == 0
        finally:
            server._workspace_lock.release()

        # Verify the cache hit path was actually taken (lock_wait_ms ~0).
        log_path = runtime_root / "checker-state" / "server.log"
        records = [
            json.loads(line)
            for line in log_path.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        cache_hits = [
            r for r in records
            if r.get("op") == "lean_compile_node"
            and r.get("compile_cache_hit") is True
        ]
        assert cache_hits, "expected at least one cache-hit log entry for Foo"
        # The cache-hit record's lake_lock_wait_ms must be 0 — the request
        # never tried to acquire the lake lock.
        assert cache_hits[-1].get("lake_lock_wait_ms") == 0, (
            f"cache-hit request waited on lake lock: {cache_hits[-1]!r}"
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


def test_handle_request_logs_lock_wait_fields(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Optimization #2 instrumentation: the request log must include
    ``sync_lock_wait_ms`` and ``lake_lock_wait_ms`` so post-run analysis
    can confirm cache-hit responses are bypassing the lake lock.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    monkeypatch.setattr(
        server_mod.observations,
        "build_tablet",
        lambda repo, *, timeout_secs, bwrap_role=None: {
            "returncode": 0, "stdout": "", "stderr": "",
            "timed_out": False, "spawn_error": "",
        },
    )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        _round_trip(server, {"op": "lean_build_tablet", "request_id": 100})
    finally:
        server.shutdown()
        thread.join(timeout=5.0)
    log_path = runtime_root / "checker-state" / "server.log"
    records = [
        json.loads(line)
        for line in log_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    bt_records = [r for r in records if r.get("op") == "lean_build_tablet"]
    assert bt_records
    rec = bt_records[0]
    assert "sync_lock_wait_ms" in rec
    assert "lake_lock_wait_ms" in rec
    assert isinstance(rec["sync_lock_wait_ms"], int)
    assert isinstance(rec["lake_lock_wait_ms"], int)


# --------------------------------------------------------------------------
# Optimization #3: pre-warm _oleans_known_current at server start
# --------------------------------------------------------------------------


def test_start_prewarms_oleans_known_current_for_current_nodes(
    runtime_root: Path,
) -> None:
    """Optimization #3: ``start()`` scans the supervisor's olean tree
    and seeds ``_oleans_known_current`` with nodes whose olean is
    current with its source. The first wave of requests after a server
    restart can then short-circuit without re-running lake on
    already-current nodes.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    # Seed supervisor Tablet/ + .lake/build/.../Tablet/ with a mix of
    # current and stale-olean nodes so we can verify the filter.
    super_tablet = server.supervisor_repo / "Tablet"
    super_tablet.mkdir(parents=True, exist_ok=True)
    olean_dir = (
        server.supervisor_repo / ".lake" / "build" / "lib" / "lean" / "Tablet"
    )
    olean_dir.mkdir(parents=True, exist_ok=True)

    # Fresh: source older than olean.
    (super_tablet / "Fresh.lean").write_text("-- src\n", encoding="utf-8")
    (olean_dir / "Fresh.olean").write_bytes(b"olean-fresh")
    # Bump olean mtime ahead of source so the prewarm includes it.
    fresh_src_mtime = (super_tablet / "Fresh.lean").stat().st_mtime
    os.utime(olean_dir / "Fresh.olean", (fresh_src_mtime + 1, fresh_src_mtime + 1))

    # Stale: olean older than source. Must NOT be in pre-warm.
    (olean_dir / "Stale.olean").write_bytes(b"olean-stale")
    stale_src_mtime = time.time()
    os.utime(olean_dir / "Stale.olean", (stale_src_mtime - 100, stale_src_mtime - 100))
    (super_tablet / "Stale.lean").write_text("-- src\n", encoding="utf-8")

    # Empty olean: must not be admitted (size 0 fails the gate).
    (super_tablet / "Empty.lean").write_text("-- src\n", encoding="utf-8")
    (olean_dir / "Empty.olean").write_bytes(b"")

    # Source with no olean: skipped silently.
    (super_tablet / "Sourceless.lean").write_text("-- src\n", encoding="utf-8")

    # Olean with no source: not in iteration set (we iterate by source).

    # Bad node name: must be skipped by the regex filter.
    (super_tablet / "0Bad.lean").write_text("-- src\n", encoding="utf-8")
    (olean_dir / "0Bad.olean").write_bytes(b"olean")

    server.set_expected_peer_uid(os.geteuid())
    server.start()
    try:
        assert "Fresh" in server._oleans_known_current
        assert "Stale" not in server._oleans_known_current
        assert "Empty" not in server._oleans_known_current
        assert "Sourceless" not in server._oleans_known_current
        assert "0Bad" not in server._oleans_known_current
    finally:
        server.shutdown()


def test_start_prewarm_handles_missing_olean_dir_gracefully(
    runtime_root: Path,
) -> None:
    """Optimization #3: if the supervisor's olean tree doesn't exist
    yet (cold workspace, first run), ``start()`` must not raise and
    must leave ``_oleans_known_current`` empty.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    # Seed Tablet/ but NOT the olean dir.
    (server.supervisor_repo / "Tablet").mkdir(parents=True, exist_ok=True)
    (server.supervisor_repo / "Tablet" / "Foo.lean").write_text(
        "-- src\n", encoding="utf-8"
    )
    server.set_expected_peer_uid(os.geteuid())
    server.start()
    try:
        assert server._oleans_known_current == set()
    finally:
        server.shutdown()


def test_start_prewarm_does_not_synthesize_compile_without_warning_fact(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Optimization #3 end-to-end: after a warm-restart with an
    already-built olean tree AND a populated sync-fingerprint cache, prewarm
    seeds the olean-current set. It must not synthesize a lean_compile_node
    response until a real compile has also populated the sorry-warning fact.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())

    # Seed worker + supervisor tablet with identical content.
    worker_tablet = server.worker_repo / "Tablet"
    worker_tablet.mkdir(parents=True, exist_ok=True)
    src_text = "import Tablet.Preamble\n"
    (worker_tablet / "Stub.lean").write_text(src_text, encoding="utf-8")
    super_tablet = server.supervisor_repo / "Tablet"
    super_tablet.mkdir(parents=True, exist_ok=True)
    (super_tablet / "Stub.lean").write_text(src_text, encoding="utf-8")
    olean_dir = (
        server.supervisor_repo / ".lake" / "build" / "lib" / "lean" / "Tablet"
    )
    olean_dir.mkdir(parents=True, exist_ok=True)
    (olean_dir / "Stub.olean").write_bytes(b"warm-olean")
    src_mtime = (super_tablet / "Stub.lean").stat().st_mtime
    os.utime(olean_dir / "Stub.olean", (src_mtime + 1, src_mtime + 1))

    # Pre-populate the sync-fingerprint cache so sync's fast-skip path
    # hits on Step A (mtime+size match) — without this, sync would
    # rewrite the supervisor's source via atomic_write, bumping its
    # mtime above the olean's and invalidating the prewarm. The live
    # supervisor's fingerprint cache is similarly persistent across
    # restarts, so this mirrors production conditions.
    import hashlib as _hashlib
    worker_src_path = worker_tablet / "Stub.lean"
    worker_st = worker_src_path.stat()
    worker_sha = _hashlib.sha256(worker_src_path.read_bytes()).hexdigest()
    runtime_state_dir = runtime_root / "checker-state"
    runtime_state_dir.mkdir(parents=True, exist_ok=True)
    fp_cache_path = runtime_state_dir / "sync-fingerprints.json"
    fp_cache_path.write_text(
        json.dumps({
            "Stub.lean": {
                "mtime_ns": int(worker_st.st_mtime_ns),
                "size": int(worker_st.st_size),
                "sha256": worker_sha,
            }
        }),
        encoding="utf-8",
    )

    call_count = {"n": 0}
    def _fake_compile_node(repo, node_name, *, timeout_secs, bwrap_role=None):
        call_count["n"] += 1
        return {
            "returncode": 0, "stdout": "", "stderr": "",
            "timed_out": False, "spawn_error": "",
            "node": node_name,
            "requested_nodes": [node_name],
            "materialized_nodes": [node_name],
        }
    monkeypatch.setattr(server_mod.observations, "compile_node", _fake_compile_node)

    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        # Confirm pre-warm seeded the cache.
        assert "Stub" in server._oleans_known_current
        # First request must not hit the prewarmed cache for lean_compile_node:
        # the server knows the olean is current, but does not yet know whether
        # Lean's compile output had a sorry warning.
        r1 = _round_trip(
            server, {"op": "lean_compile_node", "request_id": 1, "node_name": "Stub"}
        )
        assert r1["returncode"] == 0
        assert call_count["n"] == 1, (
            "first compile after warm restart must seed the warning fact"
        )
        # The real compile seeded warning status, so a second identical request
        # may now synthesize the compile result.
        r2 = _round_trip(
            server, {"op": "lean_compile_node", "request_id": 2, "node_name": "Stub"}
        )
        assert r2["returncode"] == 0
        assert call_count["n"] == 1, (
            "second compile after warning fact is known should hit cache"
        )
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


# ----------------------- Patch A local-closure probe -----------------------
#
# Plan §5.9 Tier 2 (Python unit). These exercise the validator-level and
# dispatch-level wiring for the new `local_closure_axioms` op WITHOUT
# invoking real lake. End-to-end coverage requires a live checker server
# wired against a built fixture; the operator-run smoke script
# `scripts/local_closure_smoke_example-run.sh` covers that path.


def test_validate_request_local_closure_requires_node_name() -> None:
    """Plan §5.4: ``local_closure_axioms`` is in the same node-name-required
    cohort as ``verify_node`` / ``lean_compile_node`` / ``print_axioms``."""
    with pytest.raises(ProtocolError) as excinfo:
        validate_request({"op": "local_closure_axioms", "request_id": 1})
    assert excinfo.value.kind == "malformed_request"
    assert "node_name" in excinfo.value.message


def test_validate_request_local_closure_accepts_well_formed_envelope() -> None:
    req = validate_request(
        {"op": "local_closure_axioms", "request_id": 7, "node_name": "Foo"}
    )
    assert req.op == "local_closure_axioms"
    assert req.request_id == 7
    assert req.node_name == "Foo"
    # Default timeout band is preserved (server-side coercion only kicks in
    # when timeout_secs is supplied; absent ⇒ default).
    assert req.timeout_secs == protocol.TIMEOUT_SECS_DEFAULT


def test_validate_request_local_closure_rejects_repo_path() -> None:
    """Plan §2.3 trust boundary: server derives ``repo_path``; worker may
    not even pretend to. Mirrors the existing ``repo_path``-rejection test
    for ``verify_node``."""
    with pytest.raises(ProtocolError) as excinfo:
        validate_request(
            {
                "op": "local_closure_axioms",
                "request_id": 1,
                "node_name": "Foo",
                "repo_path": "/etc",
            }
        )
    assert excinfo.value.kind == "malformed_request"


def test_validate_request_local_closure_rejects_oversized_node_name() -> None:
    too_long = "A" + "a" * NODE_NAME_MAX_LEN
    with pytest.raises(ProtocolError) as excinfo:
        validate_request(
            {"op": "local_closure_axioms", "request_id": 1, "node_name": too_long}
        )
    assert excinfo.value.kind == "malformed_request"


def test_validate_request_local_closure_rejects_pathy_node_name() -> None:
    """The shared regex anchor must keep traversal/escape characters out."""
    for bad in ("Foo.Bar", "../escape", "Foo;rm", ""):
        with pytest.raises(ProtocolError) as excinfo:
            validate_request(
                {"op": "local_closure_axioms", "request_id": 1, "node_name": bad}
            )
        assert excinfo.value.kind == "malformed_request"


def test_known_ops_includes_local_closure_axioms() -> None:
    assert "local_closure_axioms" in protocol.KNOWN_OPS


def test_dispatch_op_routes_local_closure_to_handler(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Pin that ``_dispatch_op`` routes ``local_closure_axioms`` to
    ``_handle_local_closure_axioms``. We intercept the handler so this
    runs without touching lake or the script. The handler is exercised
    directly with a synthetic request object so the test is not
    sensitive to socket framing.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())

    seen: list[Any] = []

    def _fake_handler(request):
        seen.append(request)
        return (
            {
                "request_id": request.request_id,
                "node": request.node_name,
                "status": "ok",
                "root_kind": "theorem",
                "kernel_axioms": [],
                "boundary_theorems": [],
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

    monkeypatch.setattr(server, "_handle_local_closure_axioms", _fake_handler)

    # Dispatch directly via the validated request envelope so we're
    # asserting routing only — no socket / sync involvement.
    from trellis.checker.protocol import CheckerRequest

    req = CheckerRequest(
        op="local_closure_axioms",
        request_id=99,
        node_name="ExampleNode",
        timeout_secs=120.0,
    )
    response, returncode, log_extra = server._dispatch_op(req, sync_result={})

    assert len(seen) == 1, "handler must be invoked exactly once"
    assert seen[0].node_name == "ExampleNode"
    assert response["request_id"] == 99
    assert response["status"] == "ok"
    assert returncode == 0
    assert log_extra == {"local_closure_status": "ok"}


def test_handle_local_closure_axioms_surfaces_missing_script(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When the script file is absent (e.g. operator stripped scripts/
    from a deployed runtime), the handler should fail closed with a
    structured envelope instead of raising. Exercises the missing-script
    branch in ``_handle_local_closure_axioms`` without invoking lake.
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())

    # Point the handler at a path that does not exist; lake must NOT be
    # invoked. This pins the early-return in
    # ``_handle_local_closure_axioms`` (server.py:1945-1972).
    bogus_path = runtime_root / "does-not-exist.lean"
    monkeypatch.setattr(server, "_local_closure_script_path", bogus_path)

    lake_called = {"n": 0}

    def _spy_run_lake(*args: Any, **kwargs: Any) -> Any:
        lake_called["n"] += 1
        raise AssertionError("lake must not be invoked when script is missing")

    monkeypatch.setattr(server_mod.observations, "_run_lake_command", _spy_run_lake)

    from trellis.checker.protocol import CheckerRequest

    req = CheckerRequest(
        op="local_closure_axioms",
        request_id=11,
        node_name="Foo",
        timeout_secs=120.0,
    )
    response, returncode, log_extra = server._handle_local_closure_axioms(req)

    assert lake_called["n"] == 0
    assert response["status"] == "internal_error"
    assert response["request_id"] == 11
    assert response["node"] == "Foo"
    assert any("script not found" in e for e in response["errors"])
    assert returncode is None
    assert log_extra.get("local_closure_script_missing") is True


def test_handle_local_closure_axioms_parses_script_envelope(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Stub ``_run_lake_command`` to emit a script-shaped JSON line on
    stdout; assert the handler preserves the script's fields verbatim
    (plan §5.3).
    """
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    # Force the handler to think the script is available without
    # requiring scripts/ on disk in the per-test runtime root.
    fake_script = runtime_root / "fake-local-closure.lean"
    fake_script.write_text("-- placeholder for test\n", encoding="utf-8")
    monkeypatch.setattr(server, "_local_closure_script_path", fake_script)

    script_payload = {
        "node": "Foo",
        "status": "ok",
        "root_kind": "theorem",
        "kernel_axioms": ["Classical.choice"],
        "boundary_theorems": [
            {"name": "Tablet.Helper", "statement_hash": "h1"},
        ],
        "strict_theorem_deps": [],
        "strict_definition_deps": [],
        "errors": [],
    }

    def _fake_run_lake(repo, args, *, timeout_secs, bwrap_role=None):
        # Prepend an unrelated info line on stdout so we exercise the
        # "take last non-empty line" parser branch in the handler.
        stdout = (
            "info: building Tablet.Foo (deps: ...)\n"
            + json.dumps(script_payload)
            + "\n"
        )
        return {
            "returncode": 0,
            "stdout": stdout,
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(server_mod.observations, "_run_lake_command", _fake_run_lake)

    from trellis.checker.protocol import CheckerRequest

    req = CheckerRequest(
        op="local_closure_axioms",
        request_id=33,
        node_name="Foo",
        timeout_secs=120.0,
    )
    response, returncode, log_extra = server._handle_local_closure_axioms(req)

    assert response["status"] == "ok"
    assert response["root_kind"] == "theorem"
    assert response["kernel_axioms"] == ["Classical.choice"]
    assert response["boundary_theorems"] == [
        {"name": "Tablet.Helper", "statement_hash": "h1"},
    ]
    assert response["errors"] == []
    assert response["request_id"] == 33
    assert response["node"] == "Foo"
    assert returncode == 0
    assert log_extra["local_closure_status"] == "ok"
    assert log_extra["local_closure_kernel_axioms"] == 1
    assert log_extra["local_closure_boundary_theorems"] == 1


def test_handle_local_closure_axioms_handles_malformed_stdout(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When the script writes garbage on stdout (e.g. a Lean panic),
    the handler must compose a fail-closed envelope with a useful
    diagnostic in ``errors`` rather than letting the JSON parser
    bubble up."""
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    fake_script = runtime_root / "fake-local-closure.lean"
    fake_script.write_text("-- placeholder for test\n", encoding="utf-8")
    monkeypatch.setattr(server, "_local_closure_script_path", fake_script)

    def _garbage_lake(repo, args, *, timeout_secs, bwrap_role=None):
        return {
            "returncode": 0,
            "stdout": "this is not JSON\nneither is this\n",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(server_mod.observations, "_run_lake_command", _garbage_lake)

    from trellis.checker.protocol import CheckerRequest

    req = CheckerRequest(
        op="local_closure_axioms",
        request_id=44,
        node_name="Foo",
        timeout_secs=120.0,
    )
    response, _returncode, _log_extra = server._handle_local_closure_axioms(req)
    assert response["status"] == "internal_error"
    assert any("not valid JSON" in e for e in response["errors"])
    assert response["kernel_axioms"] == []
    assert response["boundary_theorems"] == []
