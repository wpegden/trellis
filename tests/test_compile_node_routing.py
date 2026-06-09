"""End-to-end tests for the ``TRELLIS_CHECKER_SOCKET`` routing branch
inside :func:`trellis.atomic_actions.observations.compile_node`.

This is the FIRST PR that changes production behavior (when the env var
is set), so the tests cover both arms of the new branch:

- env unset -> direct-lake path (production today, byte-equivalent).
- env set + working server -> RPC client path.
- env set + missing/refused socket -> ``CheckerRpcError`` propagates;
  no silent fall-through to direct lake. This pins the no-fallback
  decision documented inline in ``compile_node``.

Plus a small sandbox passthrough check so worker bursts inherit the
socket path through the bwrap boundary.
"""

from __future__ import annotations

import os
import tempfile
import threading
from pathlib import Path
from typing import Any, Iterator

import pytest

import trellis.atomic_actions.observations as observations
from trellis.atomic_actions.checker_client import (
    CheckerRpcError,
    TRELLIS_CHECKER_SOCKET_ENV,
)
from trellis.checker import server as server_mod
from trellis.checker.server import CheckerServer
from trellis.config import SandboxConfig
from trellis.sandbox import _PASSTHROUGH_VALUE_ENV_VARS, wrap_command


# -------------------------------- fixtures --------------------------------


@pytest.fixture
def runtime_root() -> Iterator[Path]:
    """Build a runtime root layout the server expects.

    Mirrors the layout in ``tests/test_checker_client.py``: short
    system-temp path because the AF_UNIX socket path cap is 108 bytes
    and pytest's ``tmp_path`` is too deep.
    """
    base = Path(tempfile.mkdtemp(prefix="cnr-", dir="/tmp"))
    repo = base / "r"
    runtime = repo / ".trellis" / "runtime" / "rt"
    runtime.mkdir(parents=True)
    (repo / "Tablet").mkdir(parents=True)
    yield runtime
    import shutil

    shutil.rmtree(base, ignore_errors=True)


def _stub_server_compile(monkeypatch: pytest.MonkeyPatch) -> dict[str, list[dict[str, Any]]]:
    """Patch ``materialize_tablet_oleans`` (the helper the direct-lake
    arm of ``compile_node`` calls) so the server-side dispatch lands on
    a deterministic stub instead of trying to invoke real lake.

    Why patch ``materialize_tablet_oleans`` rather than ``compile_node``
    itself? The new routing branch in ``compile_node`` skips RPC when
    the caller passed ``bwrap_role="lake_compiler"`` (the supervisor's
    self-call from the server handler). Patching ``compile_node``
    directly would replace the routing function on the worker side too,
    bypassing the exact code we're trying to test. Patching the inner
    helper preserves the worker-side routing path while still giving
    the server a fast deterministic stub.

    Returns a recording dict so tests can verify dispatch routed
    through the supervisor's ``compile_node`` with the expected
    ``bwrap_role``.
    """
    seen: dict[str, list[dict[str, Any]]] = {"materialize_tablet_oleans": []}

    def _fake_materialize(
        repo: Path,
        nodes: list[str],
        *,
        timeout_secs: float,
        bwrap_role: Any = None,
    ) -> dict[str, Any]:
        seen["materialize_tablet_oleans"].append(
            {
                "repo": repo,
                "nodes": list(nodes),
                "timeout_secs": timeout_secs,
                "bwrap_role": bwrap_role,
            }
        )
        node_list = [str(n) for n in nodes]
        return {
            "requested_nodes": node_list,
            "materialized_nodes": node_list,
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(
        server_mod.observations, "materialize_tablet_oleans", _fake_materialize
    )
    return seen


@pytest.fixture
def stubbed_server(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> Iterator[tuple[CheckerServer, dict[str, list[dict[str, Any]]]]]:
    """In-process server with monkey-patched ``compile_node`` observation.

    Yields (server, seen_calls). Tests can read ``seen_calls`` to
    confirm dispatch routed through the server's compile_node.
    """
    seen = _stub_server_compile(monkeypatch)
    server = CheckerServer(runtime_root, parallelism=2, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    # Seed at least one tablet file so sync has something to mirror.
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


# ------------------------ a. env-unset direct-lake path ------------------------


def test_compile_node_raises_when_env_unset(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """With ``TRELLIS_CHECKER_SOCKET`` unset and ``bwrap_role=None``,
    ``compile_node`` must RAISE (the checker socket is mandatory for
    acceptance; the legacy direct-host-lake fallback was removed). Neither
    ``_run_lake_command`` nor the RPC client may be invoked.
    """
    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)

    repo = tmp_path / "repo"
    tablet = repo / "Tablet"
    tablet.mkdir(parents=True)
    (tablet / "Preamble.lean").write_text(
        "import Mathlib.Data.Nat.Basic\n", encoding="utf-8"
    )
    (tablet / "A.lean").write_text(
        "import Tablet.Preamble\n", encoding="utf-8"
    )

    direct_calls: list[list[str]] = []

    def _fake_run_lake_command(
        _repo: Path,
        args: list[str],
        *,
        timeout_secs: float,
        bwrap_role: Any = None,
    ) -> dict[str, Any]:
        direct_calls.append(list(args))
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    rpc_calls: list[Any] = []

    def _fake_client_compile_node(*args: Any, **kwargs: Any) -> dict[str, Any]:
        rpc_calls.append((args, kwargs))
        raise AssertionError("client_compile_node must NOT be called when env var is unset")

    monkeypatch.setattr(observations, "_run_lake_command", _fake_run_lake_command)
    monkeypatch.setattr(
        "trellis.atomic_actions.checker_client.client_compile_node",
        _fake_client_compile_node,
    )

    with pytest.raises(RuntimeError, match="checker socket required"):
        observations.compile_node(repo, "A")

    # No host-lake fallback and no RPC client invocation when the socket
    # env is unset.
    assert direct_calls == [], "host-lake fallback must not run when socket unset"
    assert rpc_calls == [], "client_compile_node must NOT be invoked when env unset"


# ------------------------ recursion guard ------------------------


def test_compile_node_skips_rpc_when_bwrap_role_set(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """When ``bwrap_role`` is set (i.e. the supervisor's checker server is
    invoking ``compile_node`` to do the actual lake work), the routing
    branch must be skipped — otherwise the supervisor would recurse
    through RPC infinitely. Worker callers always pass ``bwrap_role=None``
    so this guard cleanly distinguishes the two roles even when
    ``TRELLIS_CHECKER_SOCKET`` is set on the supervisor.
    """
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, "/tmp/should-not-be-dialed.sock")

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet" / "X.lean").write_text("import Tablet.Preamble\n", encoding="utf-8")
    (repo / "Tablet" / "Preamble.lean").write_text("", encoding="utf-8")

    direct_calls: list[list[str]] = []

    def _fake_run_lake_command(
        _repo: Path,
        args: list[str],
        *,
        timeout_secs: float,
        bwrap_role: Any = None,
    ) -> dict[str, Any]:
        direct_calls.append(list(args))
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    def _fake_client_compile_node(*args: Any, **kwargs: Any) -> dict[str, Any]:
        raise AssertionError(
            "client_compile_node must NOT be called when bwrap_role is set "
            "(supervisor's lake invocation must take the direct path)"
        )

    monkeypatch.setattr(observations, "_run_lake_command", _fake_run_lake_command)
    monkeypatch.setattr(
        "trellis.atomic_actions.checker_client.client_compile_node",
        _fake_client_compile_node,
    )

    payload = observations.compile_node(repo, "X", bwrap_role="lake_compiler")

    assert direct_calls, "expected direct-lake path even with env var set"
    assert payload["node"] == "X"
    assert payload["returncode"] == 0


# ------------------------ b. env-set RPC routing ------------------------


def test_compile_node_routes_through_client_when_env_set(
    stubbed_server: tuple[CheckerServer, dict[str, list[dict[str, Any]]]],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """With ``TRELLIS_CHECKER_SOCKET`` set to a working in-process
    server's socket, ``compile_node`` must round-trip through the RPC
    client and return the server's response. ``_run_lake_command`` must
    NOT be invoked locally — that's the entire point of routing.
    """
    server, seen = stubbed_server
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))

    direct_calls: list[Any] = []

    def _fake_run_lake_command(*args: Any, **kwargs: Any) -> dict[str, Any]:
        direct_calls.append((args, kwargs))
        raise AssertionError(
            "_run_lake_command must NOT be called when routing through RPC"
        )

    monkeypatch.setattr(observations, "_run_lake_command", _fake_run_lake_command)

    payload = observations.compile_node(
        server.supervisor_repo,
        "LemmaA",
        timeout_secs=30.0,
    )

    # RPC was used; direct lake was not on the worker side. The server
    # also did NOT call ``_run_lake_command`` directly because we
    # patched ``materialize_tablet_oleans`` (which is the helper the
    # supervisor-side direct-lake fall-through invokes).
    assert direct_calls == [], (
        "_run_lake_command must NOT be invoked when env var routes through RPC"
    )
    # Server-side compile_node delegated to materialize_tablet_oleans
    # exactly once with the worker-supplied node name and the
    # supervisor's bwrap_role.
    assert len(seen["materialize_tablet_oleans"]) == 1
    server_call = seen["materialize_tablet_oleans"][0]
    assert server_call["nodes"] == ["LemmaA"]
    # The server's compile_node passes bwrap_role="lake_compiler" through
    # to materialize_tablet_oleans on its own side.
    assert server_call["bwrap_role"] == "lake_compiler"

    # Caller sees the same response shape it sees today (request_id stripped).
    assert payload["node"] == "LemmaA"
    assert payload["returncode"] == 0
    assert payload["stderr"] == ""
    assert payload["timed_out"] is False
    assert payload["spawn_error"] == ""
    assert payload["materialized_nodes"] == ["LemmaA"]
    assert payload["requested_nodes"] == ["LemmaA"]
    # request_id is an internal protocol detail; the direct-lake path
    # does not emit it, so the routing branch must strip it for shape
    # parity.
    assert "request_id" not in payload


# ------------------ c. response-shape parity across both modes ------------------


def test_compile_node_response_shape_identical_in_both_modes(
    stubbed_server: tuple[CheckerServer, dict[str, list[dict[str, Any]]]],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The direct-lake path and the RPC path must return dicts with the
    SAME key set so callers don't have to know which path served them.

    Uses a real in-process server for the RPC arm; mocks
    ``_run_lake_command`` for the direct arm. Both arms compile the
    same node so the assertion is on key-set equality, not value
    equality (the RPC server's stub emits slightly different stdout
    text from the direct arm's lake stub).
    """
    server, _seen = stubbed_server

    # ---- direct-lake arm (bwrap_role="lake_compiler" = server endpoint) ----
    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)
    repo_direct = tmp_path / "repo_direct"
    tablet_direct = repo_direct / "Tablet"
    tablet_direct.mkdir(parents=True)
    (tablet_direct / "Preamble.lean").write_text(
        "import Mathlib.Data.Nat.Basic\n", encoding="utf-8"
    )
    (tablet_direct / "LemmaA.lean").write_text(
        "import Tablet.Preamble\n", encoding="utf-8"
    )

    def _fake_run_lake_command(
        _repo: Path,
        args: list[str],
        *,
        timeout_secs: float,
        bwrap_role: Any = None,
    ) -> dict[str, Any]:
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", _fake_run_lake_command)
    direct_payload = observations.compile_node(
        repo_direct, "LemmaA", bwrap_role="lake_compiler"
    )
    direct_keys = set(direct_payload.keys())

    # ---- RPC arm ----
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))
    # We must restore _run_lake_command so the RPC arm's local code path
    # never errors; in practice the RPC routing should never call it,
    # but if it did we'd hit the AssertionError below — the assertion
    # would still let us prove the shape.
    rpc_payload = observations.compile_node(
        server.supervisor_repo,
        "LemmaA",
        timeout_secs=30.0,
    )
    rpc_keys = set(rpc_payload.keys())

    # Both code paths emit the same key set. The direct-lake path adds
    # the canonical observation keys; the RPC path strips request_id and
    # emits the same canonical set.
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
    assert expected_keys.issubset(direct_keys), (
        f"direct-lake path is missing canonical keys: {expected_keys - direct_keys}"
    )
    assert expected_keys.issubset(rpc_keys), (
        f"RPC path is missing canonical keys: {expected_keys - rpc_keys}"
    )
    # Symmetric difference should be empty: every key in one is in the other.
    assert direct_keys == rpc_keys, (
        f"key sets differ: direct={direct_keys}, rpc={rpc_keys}, "
        f"diff={direct_keys.symmetric_difference(rpc_keys)}"
    )


# ------------------ d. supervisor_unavailable propagation ------------------


def test_compile_node_supervisor_unavailable_propagates(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """When ``TRELLIS_CHECKER_SOCKET`` points at a missing socket the
    routing branch must raise ``CheckerRpcError(kind="supervisor_unavailable")``
    rather than silently fall through to direct lake.

    Pins the no-fallback decision: the operator opted in by setting the
    flag; surfacing the misconfiguration is the right call. Silent
    fallback would let the worker run lake locally — causing widget /
    .lake / build writes the supervisor wasn't expecting — and break
    the trust model the env var is supposed to enforce.
    """
    bogus_socket = tmp_path / "nonexistent_dir" / "checker.sock"
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(bogus_socket))

    direct_calls: list[Any] = []

    def _fake_run_lake_command(*args: Any, **kwargs: Any) -> dict[str, Any]:
        direct_calls.append((args, kwargs))
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", _fake_run_lake_command)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    with pytest.raises(CheckerRpcError) as excinfo:
        observations.compile_node(repo, "A", timeout_secs=2.0)

    assert excinfo.value.kind == "supervisor_unavailable"
    assert direct_calls == [], (
        "_run_lake_command must NOT be invoked when env var is set but socket missing"
    )


# ----------------- e. sandbox env-var passthrough wiring -----------------


def test_passthrough_value_envs_includes_check_socket() -> None:
    """``TRELLIS_CHECKER_SOCKET`` must be in the passthrough tuple so
    worker bursts inherit it through the bwrap boundary. Without this,
    setting the env on the supervisor has no effect on the worker's
    in-burst observation calls.
    """
    assert "TRELLIS_CHECKER_SOCKET" in _PASSTHROUGH_VALUE_ENV_VARS


def test_wrap_command_propagates_check_socket(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """End-to-end: when ``TRELLIS_CHECKER_SOCKET`` is set on the
    supervisor, the bwrap argv must include ``--setenv
    TRELLIS_CHECKER_SOCKET <path>``. This pins the contract that the
    inside-burst check.py sees the same socket path the supervisor set.
    """
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    sock_path = "/tmp/trellis-test/checker.sock"
    monkeypatch.setenv("TRELLIS_CHECKER_SOCKET", sock_path)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )

    # Argv must contain the setenv triplet for the socket env var.
    needle = ["--setenv", "TRELLIS_CHECKER_SOCKET", sock_path]
    found = False
    for idx in range(len(cmd) - 2):
        if cmd[idx : idx + 3] == needle:
            found = True
            break
    assert found, (
        f"bwrap argv must include --setenv TRELLIS_CHECKER_SOCKET {sock_path}; "
        f"got: {cmd}"
    )


def test_wrap_command_omits_check_socket_when_unset(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Symmetric to the propagation test: when the env var is unset on
    the supervisor, the bwrap argv must NOT inject a stray setenv.
    Otherwise the worker burst would see a phantom value (or empty
    string) and route through a non-existent socket.
    """
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.delenv("TRELLIS_CHECKER_SOCKET", raising=False)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    cmd = wrap_command(
        ["echo", "ok"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=repo,
        burst_home=Path("/home/sandbox-user"),
        role="worker",
    )

    # No setenv should reference TRELLIS_CHECKER_SOCKET when env unset.
    for idx in range(len(cmd) - 1):
        if cmd[idx] == "--setenv" and cmd[idx + 1] == "TRELLIS_CHECKER_SOCKET":
            pytest.fail(
                f"bwrap argv must NOT inject --setenv TRELLIS_CHECKER_SOCKET "
                f"when env unset; got: {cmd}"
            )
