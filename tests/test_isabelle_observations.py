"""Tests for the Isabelle observation envelope synthesis + the AF_UNIX
trust boundary preservation for the Isabelle ops (checker B2a).

Covers:

* the ``LocalClosureProbeOutput`` cert envelope (``thm_oracles`` →
  ``oracles_used``, ``thm_deps`` → ``kernel_axioms``, ``theorem_exists``,
  status) and the ``ExternalCommandObservation`` envelope (returncode 0
  iff ok && failed==0);
* the cert envelope shape parses cleanly under the kernel's documented
  contract (string arrays for axioms/oracles, ``{name, <hash>}`` arrays
  for boundary/dep lists);
* THE AF_UNIX TRUST TEST — reusing the checker-server harness, a
  request-supplied ``repo_path`` is REJECTED for every new Isabelle op,
  and the repo the server uses is derived from the socket runtime root;
* ``_dispatch_op`` routes the Isabelle ops to their handlers, and the
  scaffold-missing path fails closed (no real ``isabelle`` spawned).
"""

from __future__ import annotations

import json
import os
import socket
import tempfile
import threading
from pathlib import Path
from typing import Any, Dict, Iterator, List

import pytest

from trellis.atomic_actions import isabelle_observations as iso
from trellis.checker import protocol, server as server_mod
from trellis.checker.isabelle_session import CheckOutcome
from trellis.checker.protocol import ProtocolError, validate_request
from trellis.checker.server import CheckerServer


# ============================ envelope synthesis ============================


def test_external_command_envelope_shape() -> None:
    env = iso.external_command_envelope(returncode=0, stdout="x", stderr="y")
    assert set(env) == {"returncode", "stdout", "stderr", "timed_out", "spawn_error"}
    assert env["returncode"] == 0
    assert env["timed_out"] is False


def test_cert_envelope_clean_proof_maps_to_ok() -> None:
    outcome = CheckOutcome(
        ok=True,
        failed=0,
        finished=10,
        theory_name="Draft.Tablet_Triv",
        node_name="/tmp/Tablet_Triv.thy",
        oracles=[],
        dependencies=["One_nat_def", "refl"],
        theorem_exists=True,
    )
    cert = iso.local_closure_cert_envelope(outcome)
    assert cert["status"] == iso.STATUS_OK
    assert cert["oracles_used"] == []
    assert cert["kernel_axioms"] == ["One_nat_def", "refl"]
    assert cert["theorem_exists"] is True
    assert cert["returncode"] == 0
    assert cert["errors"] == []


def test_cert_envelope_sorry_surfaces_oracle_but_stays_parseable() -> None:
    """A ``sorry`` proof checks (ok) but taints with ``skip_proof``; the
    cert surfaces the oracle in ``oracles_used`` for the B2c-gate to
    reject. ``status`` is still ``ok`` (the proof did 'check'); the gate —
    not the observation — rejects on the non-empty oracle set."""
    outcome = CheckOutcome(
        ok=True, failed=0, finished=10,
        theory_name="Draft.T", node_name="/tmp/T.thy",
        oracles=["skip_proof"], dependencies=["refl"], theorem_exists=True,
    )
    cert = iso.local_closure_cert_envelope(outcome)
    assert cert["oracles_used"] == ["skip_proof"]
    assert cert["status"] == iso.STATUS_OK


def test_cert_envelope_failed_proof_is_invalid() -> None:
    outcome = CheckOutcome(
        ok=False, failed=1, finished=9,
        theory_name="Draft.Bad", node_name="/tmp/Bad.thy",
        error_lines=["Failed to finish proof"],
        oracles=[], dependencies=[], theorem_exists=True,
    )
    cert = iso.local_closure_cert_envelope(outcome)
    assert cert["status"] == iso.STATUS_INVALID_PROOF
    assert cert["returncode"] != 0
    assert any("Failed to finish proof" in e for e in cert["errors"])


def test_cert_envelope_missing_theorem_is_internal_error() -> None:
    outcome = CheckOutcome(
        ok=True, failed=0, finished=10,
        theory_name="Draft.T", node_name="/tmp/T.thy",
        oracles=[], dependencies=["refl"], theorem_exists=False,
    )
    cert = iso.local_closure_cert_envelope(outcome)
    assert cert["status"] == iso.STATUS_INTERNAL_ERROR
    assert any("oops/elision" in e or "absent" in e for e in cert["errors"])


def test_internal_error_cert_fail_closed() -> None:
    cert = iso._internal_error_cert("boom")
    assert cert["status"] == iso.STATUS_INTERNAL_ERROR
    assert cert["kernel_axioms"] == []
    assert cert["oracles_used"] == []
    assert cert["returncode"] is None
    assert cert["errors"] == ["boom"]


def test_cert_envelope_matches_kernel_contract_shapes() -> None:
    """The cert's list shapes must match what the kernel's
    ``parse_local_closure_response`` expects: ``kernel_axioms`` /
    ``oracles_used`` / ``errors`` are string arrays; the boundary / dep
    lists are arrays of ``{"name", "<hash_field>"}`` objects."""
    outcome = CheckOutcome(
        ok=True, failed=0, finished=10, theory_name="T", node_name="n",
        oracles=["z3"], dependencies=["refl"], theorem_exists=True,
    )
    cert = iso.local_closure_cert_envelope(outcome)
    assert all(isinstance(x, str) for x in cert["kernel_axioms"])
    assert all(isinstance(x, str) for x in cert["oracles_used"])
    # The cert is JSON-serializable (the wire requires it).
    round_tripped = json.loads(json.dumps(cert))
    for key, hash_field in (
        ("boundary_theorems", "statement_hash"),
        ("strict_theorem_deps", "value_hash"),
        ("strict_definition_deps", "semantic_hash"),
    ):
        for entry in round_tripped[key]:
            assert "name" in entry and hash_field in entry


# ============================ server-side drivers ============================


class _StubSession:
    """A session stub that returns a scripted CheckOutcome (no TCP)."""

    def __init__(self, outcome: CheckOutcome) -> None:
        self._outcome = outcome
        self.calls: List[Dict[str, Any]] = []

    def check_theory(self, **kwargs: Any) -> CheckOutcome:
        self.calls.append(kwargs)
        return self._outcome


def test_check_node_server_side_returncode_zero_on_ok() -> None:
    outcome = CheckOutcome(ok=True, failed=0, finished=10, theory_name="T",
                           node_name="n", writeln_lines=["theorem triv: ..."])
    sess = _StubSession(outcome)
    env = iso.check_node_server_side(sess, master_dir="/tmp", theory="Tablet_Triv",
                                     cert_theorem="triv")
    assert env["returncode"] == 0
    assert sess.calls[0]["theory"] == "Tablet_Triv"


def test_thm_deps_server_side_produces_cert(tmp_path: Path) -> None:
    outcome = CheckOutcome(ok=True, failed=0, finished=10, theory_name="T",
                           node_name="n", oracles=[], dependencies=["refl"],
                           theorem_exists=True)
    sess = _StubSession(outcome)
    cert = iso.thm_deps_server_side(sess, master_dir=str(tmp_path),
                                    theory="Tablet_Triv", cert_theorem="triv")
    assert cert["status"] == iso.STATUS_OK
    assert cert["kernel_axioms"] == ["refl"]
    # The two-step S1 probe ran: worker check + probe check (same stub
    # outcome), and the checker-owned probe theory was written.
    assert len(sess.calls) == 2
    assert (tmp_path / "Tablet_Triv__Cert.thy").is_file()


def test_thm_oracles_server_side_renders_stdout_surface(tmp_path: Path) -> None:
    outcome = CheckOutcome(ok=True, failed=0, finished=10, theory_name="T",
                           node_name="n", oracles=["skip_proof"],
                           dependencies=["refl"], theorem_exists=True)
    sess = _StubSession(outcome)
    env = iso.thm_oracles_server_side(sess, master_dir=str(tmp_path),
                                      theory="Tablet_T", cert_theorem="t")
    assert env["returncode"] == 0
    assert "skip_proof" in env["stdout"]
    assert "refl" in env["stdout"]


# ============================ protocol validation ============================


def test_known_ops_includes_isabelle_ops() -> None:
    for op in (
        "isabelle_check_node",
        "isabelle_thm_oracles",
        "isabelle_thm_deps",
        "isabelle_build_session",
        "isabelle_sync_session",
    ):
        assert op in protocol.KNOWN_OPS


def test_isabelle_node_ops_require_node_name() -> None:
    for op in protocol.ISABELLE_NODE_OPS:
        with pytest.raises(ProtocolError) as exc:
            validate_request({"op": op, "request_id": 1})
        assert exc.value.kind == "malformed_request"
        # With a valid node name it parses.
        req = validate_request({"op": op, "request_id": 1, "node_name": "FooNode"})
        assert req.node_name == "FooNode"


def test_isabelle_workspace_ops_take_no_node() -> None:
    for op in protocol.ISABELLE_WORKSPACE_OPS:
        req = validate_request({"op": op, "request_id": 1})
        assert req.node_name is None


@pytest.mark.parametrize(
    "op",
    [
        "isabelle_check_node",
        "isabelle_thm_oracles",
        "isabelle_thm_deps",
        "isabelle_build_session",
        "isabelle_sync_session",
    ],
)
def test_isabelle_ops_reject_repo_path_field(op: str) -> None:
    """THE TRUST PROPERTY at the protocol layer: ``repo_path`` is rejected
    for every Isabelle op exactly as for the Lean ops — the server derives
    the repo from the socket location, never from a request field."""
    payload: Dict[str, Any] = {"op": op, "request_id": 1, "repo_path": "/etc"}
    if op in protocol.ISABELLE_NODE_OPS:
        payload["node_name"] = "FooNode"
    with pytest.raises(ProtocolError) as exc:
        validate_request(payload)
    assert exc.value.kind == "malformed_request"
    assert "repo_path" in exc.value.message


# ============================ dispatch routing ============================


@pytest.fixture
def runtime_root() -> Iterator[Path]:
    base = Path(tempfile.mkdtemp(prefix="isa-obs-", dir="/tmp"))
    repo = base / "r"
    runtime = repo / ".trellis" / "runtime" / "rt"
    runtime.mkdir(parents=True)
    (repo / "Tablet").mkdir(parents=True)
    yield runtime
    import shutil

    shutil.rmtree(base, ignore_errors=True)


def test_dispatch_routes_isabelle_thm_deps_to_handler(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    seen: List[Any] = []

    def _fake(request):
        seen.append(request)
        return ({"request_id": request.request_id, "node": request.node_name,
                 "status": "ok"}, 0, {"isabelle_op": request.op})

    monkeypatch.setattr(server, "_handle_isabelle_node_op", _fake)
    from trellis.checker.protocol import CheckerRequest

    req = CheckerRequest(op="isabelle_thm_deps", request_id=7, node_name="N",
                         timeout_secs=60.0)
    response, rc, _ = server._dispatch_op(req, sync_result={})
    assert len(seen) == 1
    assert response["request_id"] == 7
    assert rc == 0


def test_isabelle_node_op_scaffold_missing_fails_closed(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """With no Isabelle scaffold present (the production state today — no
    IsabelleHol tablet live), the node-op handler fails closed with a
    structured envelope and NEVER spawns a real Isabelle session."""
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())

    spawned = {"n": 0}

    def _boom() -> Any:
        spawned["n"] += 1
        raise AssertionError("must not start a session when scaffold absent")

    monkeypatch.setattr(server, "_get_or_create_isabelle_session", _boom)
    from trellis.checker.protocol import CheckerRequest

    # thm_deps → cert-shaped internal_error.
    req = CheckerRequest(op="isabelle_thm_deps", request_id=3, node_name="Missing",
                         timeout_secs=60.0)
    response, rc, log_extra = server._handle_isabelle_node_op(req)
    assert spawned["n"] == 0
    assert response["status"] == "internal_error"
    assert response["node"] == "Missing"
    assert log_extra.get("isabelle_scaffold_missing") is True
    assert any("scaffold absent" in e for e in response["errors"])

    # check_node → command-shaped envelope with spawn_error set.
    req2 = CheckerRequest(op="isabelle_check_node", request_id=4, node_name="Missing",
                          timeout_secs=60.0)
    response2, rc2, _ = server._handle_isabelle_node_op(req2)
    assert response2["returncode"] is None
    assert "scaffold absent" in response2["spawn_error"]


def test_isabelle_workspace_op_build_noop_without_root(runtime_root: Path) -> None:
    """``isabelle_build_session`` (B2d) is a no-op-success when no scaffold
    ROOT exists yet (the production state — no IsabelleHol tablet live): the
    prebuilt HOL heap needs no build, so the kernel precondition path is
    satisfied without spawning ``isabelle build`` against a missing ROOT. The
    log carries the socket-derived session dir."""
    server = CheckerServer(runtime_root, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    from trellis.checker.protocol import CheckerRequest

    req = CheckerRequest(op="isabelle_build_session", request_id=5, timeout_secs=60.0)
    response, rc, log_extra = server._handle_isabelle_workspace_op(req)
    assert rc == 0
    assert response["returncode"] == 0
    assert str(server.supervisor_repo) in log_extra["isabelle_session_dir"]


# ============================ AF_UNIX trust test (end-to-end) ============================


@pytest.fixture
def started_server(runtime_root: Path) -> Iterator[CheckerServer]:
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


def _round_trip(server: CheckerServer, payload: Dict[str, Any]) -> Dict[str, Any]:
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(str(server.socket_path))
    sock.settimeout(10.0)
    try:
        sock.sendall((json.dumps(payload) + "\n").encode("utf-8"))
        data = b""
        while not data.endswith(b"\n"):
            chunk = sock.recv(65536)
            if not chunk:
                break
            data += chunk
        return json.loads(data.decode("utf-8"))
    finally:
        sock.close()


def test_af_unix_rejects_repo_path_for_isabelle_ops_end_to_end(
    started_server: CheckerServer,
) -> None:
    """End-to-end over the real AF_UNIX socket: a request-supplied
    ``repo_path`` is rejected for the Isabelle ops with the same
    ``malformed_request`` rpc_error the Lean ops produce — the trust
    boundary holds byte-identically for the new ops."""
    for op in ("isabelle_check_node", "isabelle_thm_deps", "isabelle_thm_oracles"):
        resp = _round_trip(
            started_server,
            {"op": op, "request_id": 1, "node_name": "FooNode", "repo_path": "/etc"},
        )
        assert "rpc_error" in resp, (op, resp)
        assert resp["rpc_error"]["kind"] == "malformed_request", (op, resp)
        assert "repo_path" in resp["rpc_error"]["message"], (op, resp)


def test_af_unix_isabelle_op_uses_socket_derived_repo(
    started_server: CheckerServer, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The repo the Isabelle handler resolves comes from the socket runtime
    root (``server.supervisor_repo``), NOT from any request field. We drive
    a real ``isabelle_thm_deps`` over the socket; the scaffold-missing
    handler reports the path under the socket-derived ``supervisor_repo``."""
    resp = _round_trip(
        started_server,
        {"op": "isabelle_thm_deps", "request_id": 9, "node_name": "FooNode"},
    )
    # No rpc_error: the op ran through the dispatcher.
    assert "rpc_error" not in resp, resp
    assert resp["request_id"] == 9
    assert resp["node"] == "FooNode"
    # Scaffold absent → internal_error, and the reported path is under the
    # socket-derived supervisor repo (proving the repo is socket-derived).
    assert resp["status"] == "internal_error"
    supervisor_repo = str(started_server.supervisor_repo)
    assert any(supervisor_repo in e for e in resp["errors"]), (supervisor_repo, resp)


def test_af_unix_isabelle_workspace_op_succeeds_end_to_end(
    started_server: CheckerServer,
) -> None:
    resp = _round_trip(
        started_server, {"op": "isabelle_build_session", "request_id": 12}
    )
    assert "rpc_error" not in resp, resp
    assert resp["returncode"] == 0
