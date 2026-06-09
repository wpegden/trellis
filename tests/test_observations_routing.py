"""End-to-end tests for the ``TRELLIS_CHECKER_SOCKET`` routing branch
inside the remaining observation functions cut over in Step 4 of the
unified-checker migration.

Step 3 cut over ``compile_node``; Step 4 mirrors that pattern for:
  - ``materialize_tablet_oleans``
  - ``print_axioms``
  - ``observe_lean_semantic_payloads``
  - ``build_tablet``
  - ``prepare_compiled_support``

Each op gets the same four routing tests:
  a. env unset + bwrap_role=None -> socket-mandatory raise (no host-lake
     fallback; the legacy direct-host-lake path was removed).
  b. env set + working server -> RPC client path; direct-lake NOT called.
  c. response-shape parity: same key set in both modes.
  d. recursion guard: ``bwrap_role="lake_compiler"`` skips RPC even when
     the env var is set (otherwise the supervisor-side handler would
     recurse through RPC).

Plus extra unwrap coverage for ``observe_lean_semantic_payloads`` (the
most complex shape; the wire envelope is ``{"request_id", "nodes":
{...}}`` while the direct-call return is just the per-node dict).

Stubbing strategy
-----------------
Unlike :mod:`tests.test_compile_node_routing`, where the server-side
helper ``materialize_tablet_oleans`` is patched (because ``compile_node``
delegates to it), the five ops here are themselves the top-level
observation functions. Patching them on ``server_mod.observations``
would also shadow the worker-side function (Python modules are
process-global), bypassing the routing branch under test.

We instead stub the LEAF :func:`_run_lake_command` plus a couple of
filesystem helpers. The worker's observation runs the routing branch
unaltered; on RPC, the server's recursion-guarded direct-lake path
calls ``_run_lake_command`` (stubbed) and returns canned data. With
``TRELLIS_CHECKER_SOCKET`` unset and ``bwrap_role=None``, the
observation now RAISES (socket-mandatory; no host-lake fallback) — the
stubbed ``_run_lake_command`` is asserted never to run in that case.
The RPC arm exercises the real observation code; only the lake
invocation itself is faked.
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
from trellis.checker.server import CheckerServer


# -------------------------------- fixtures --------------------------------


@pytest.fixture
def runtime_root() -> Iterator[Path]:
    """Build a runtime root layout the server expects.

    Mirrors ``tests/test_compile_node_routing.py``: short system-temp
    path because the AF_UNIX socket path cap is 108 bytes and pytest's
    ``tmp_path`` is too deep.
    """
    base = Path(tempfile.mkdtemp(prefix="obr-", dir="/tmp"))
    repo = base / "r"
    runtime = repo / ".trellis" / "runtime" / "rt"
    runtime.mkdir(parents=True)
    (repo / "Tablet").mkdir(parents=True)
    yield runtime
    import shutil

    shutil.rmtree(base, ignore_errors=True)


def _install_run_lake_stub(
    monkeypatch: pytest.MonkeyPatch,
) -> tuple[list[dict[str, Any]], Any]:
    """Patch :func:`_run_lake_command` with a fake that records every call.

    The fake recognises a couple of argv shapes so the per-node parsers
    in ``observe_lean_semantic_payloads`` and the materialize batched
    paths produce consistent results.

    For ``lake build --jobs N Tablet.X Tablet.Y ...`` invocations (the
    new batched materialize_tablet_oleans path), the stub writes a stub
    olean for each Tablet target so the post-build stat-walk surfaces
    them in ``materialized_nodes``. Without this the new code path
    would correctly report zero materialized nodes (no real lake means
    no real oleans).

    Returns ``(direct_calls, stub)`` so tests can swap the stub for an
    AssertionError raiser when they want to assert the function was
    NOT called (RPC arm).
    """
    direct_calls: list[dict[str, Any]] = []

    def _fake_run_lake_command(
        repo: Path,
        args: Any,
        *,
        timeout_secs: float,
        bwrap_role: Any = None,
    ) -> dict[str, Any]:
        argv = list(args)
        direct_calls.append(
            {
                "repo": repo,
                "args": argv,
                "timeout_secs": timeout_secs,
                "bwrap_role": bwrap_role,
            }
        )
        # If the call is the lean-semantic-fingerprint script, emit a
        # fingerprint marker so the parser populates ok=True. The script
        # path is the second-to-last arg; the node name is the last.
        if (
            len(argv) >= 4
            and argv[0] == "env"
            and argv[1] == "lean"
            and argv[2] == "--run"
        ):
            node_name = argv[-1]
            return {
                "returncode": 0,
                "stdout": f"FP\t{node_name}\tpayload-of-{node_name}\n",
                "stderr": "",
                "timed_out": False,
                "spawn_error": "",
            }
        # Batched materialize path: ``lake build [--jobs N] Tablet.X Tablet.Y ...``.
        # The new materialize_tablet_oleans uses stat-walk to detect
        # which oleans landed; without real lake we must drop stub
        # oleans so the post-call walk surfaces them.
        if argv and argv[0] == "build":
            for token in argv[1:]:
                if isinstance(token, str) and token.startswith("Tablet."):
                    node_name = token[len("Tablet."):]
                    olean = (
                        repo
                        / ".lake"
                        / "build"
                        / "lib"
                        / "lean"
                        / "Tablet"
                        / f"{node_name}.olean"
                    )
                    olean.parent.mkdir(parents=True, exist_ok=True)
                    olean.write_bytes(b"stub-olean-" + node_name.encode())
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    monkeypatch.setattr(observations, "_run_lake_command", _fake_run_lake_command)
    return direct_calls, _fake_run_lake_command


@pytest.fixture
def stubbed_server(
    runtime_root: Path, monkeypatch: pytest.MonkeyPatch
) -> Iterator[tuple[CheckerServer, list[dict[str, Any]]]]:
    """In-process server whose lake invocations are stubbed at the leaf.

    Unlike the per-observation patching in ``tests/test_checker_client.py``
    (which works there because those tests call the ``client_*`` functions
    directly, not through the routing branch), we patch the LEAF helper
    so the worker's call to e.g. ``observations.materialize_tablet_oleans``
    isn't shadowed. The supervisor-side observation runs end-to-end with a
    canned ``_run_lake_command`` underneath.

    Yields ``(server, direct_calls)``. Tests can read ``direct_calls`` to
    confirm the supervisor side actually invoked lake (vs the worker
    shorting through RPC without ever reaching it).
    """
    direct_calls, _ = _install_run_lake_stub(monkeypatch)

    server = CheckerServer(runtime_root, parallelism=2, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    # Seed the worker repo with Tablet files so the supervisor-side sync
    # produces the same set on its side; the supervisor's observation
    # function then walks the closure off these files.
    for node_name in ("A", "B", "LemmaA", "Stub"):
        (server.worker_repo / "Tablet" / f"{node_name}.lean").write_text(
            "", encoding="utf-8"
        )
    server.start()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server, direct_calls
    finally:
        server.shutdown()
        thread.join(timeout=5.0)


# ============================================================================
# materialize_tablet_oleans
# ============================================================================


def test_materialize_raises_when_env_unset(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Env unset + bwrap_role=None -> socket-mandatory raise; no host-lake.

    The legacy direct-host-lake fallback was removed: acceptance ops must
    route through the checker socket. With the socket env unset there is no
    fallback, so the observation raises and neither lake nor the RPC client
    is invoked.
    """
    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)

    repo = tmp_path / "repo"
    tablet = repo / "Tablet"
    tablet.mkdir(parents=True)
    (tablet / "A.lean").write_text("import Tablet.Preamble\n", encoding="utf-8")
    (tablet / "Preamble.lean").write_text("", encoding="utf-8")

    direct_calls, _ = _install_run_lake_stub(monkeypatch)

    def _fake_client(*args: Any, **kwargs: Any) -> dict[str, Any]:
        raise AssertionError(
            "client_materialize_tablet_oleans must NOT be called when env unset"
        )

    monkeypatch.setattr(observations, "client_materialize_tablet_oleans", _fake_client)

    with pytest.raises(RuntimeError, match="checker socket required"):
        observations.materialize_tablet_oleans(repo, ["A"])

    assert direct_calls == [], "host-lake fallback must not run when socket unset"


def test_materialize_routes_through_client_when_env_set(
    stubbed_server: tuple[CheckerServer, list[dict[str, Any]]],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Env set + working server -> RPC; worker side does NOT call lake.

    The supervisor side does call the (stubbed) ``_run_lake_command``;
    the test asserts the worker observation took the RPC path by
    checking that the supervisor's stubbed lake calls were issued with
    ``bwrap_role="lake_compiler"`` (the supervisor-side sentinel).
    """
    server, direct_calls = stubbed_server
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))

    payload = observations.materialize_tablet_oleans(
        server.supervisor_repo,
        ["A", "B"],
        timeout_secs=60.0,
    )

    # The supervisor side ran lake (stub), so direct_calls is non-empty,
    # and every recorded call has bwrap_role="lake_compiler".
    assert direct_calls, "expected supervisor side to call _run_lake_command"
    for call in direct_calls:
        assert call["bwrap_role"] == "lake_compiler", (
            "supervisor-side direct-lake call must use lake_compiler role"
        )

    assert payload["requested_nodes"] == ["A", "B"]
    assert payload["materialized_nodes"] == ["A", "B"]
    assert payload["returncode"] == 0
    assert "request_id" not in payload


def test_materialize_response_shape_identical_in_both_modes(
    stubbed_server: tuple[CheckerServer, list[dict[str, Any]]],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Direct-lake path and RPC path return dicts with the same key set.

    Both arms ultimately call the same observation code with the same
    leaf stub; the assertion is on key-set parity.
    """
    server, _direct_calls = stubbed_server

    # RPC arm first (the fixture's stub is installed for both arms).
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))
    rpc_payload = observations.materialize_tablet_oleans(
        server.supervisor_repo, ["A"], timeout_secs=60.0
    )
    rpc_keys = set(rpc_payload.keys())

    # Direct arm: env unset, separate worker repo.
    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)
    repo_direct = tmp_path / "repo_direct"
    tablet = repo_direct / "Tablet"
    tablet.mkdir(parents=True)
    (tablet / "A.lean").write_text("", encoding="utf-8")
    direct_payload = observations.materialize_tablet_oleans(
        repo_direct, ["A"], bwrap_role="lake_compiler"
    )
    direct_keys = set(direct_payload.keys())

    expected_keys = {
        "requested_nodes",
        "materialized_nodes",
        "returncode",
        "stdout",
        "stderr",
        "timed_out",
        "spawn_error",
    }
    assert expected_keys.issubset(direct_keys)
    assert expected_keys.issubset(rpc_keys)
    assert direct_keys == rpc_keys, (
        f"key sets differ: direct={direct_keys}, rpc={rpc_keys}"
    )


def test_materialize_skips_rpc_when_bwrap_role_set(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``bwrap_role="lake_compiler"`` with env set takes the direct path."""
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, "/tmp/should-not-be-dialed.sock")

    repo = tmp_path / "repo"
    tablet = repo / "Tablet"
    tablet.mkdir(parents=True)
    (tablet / "A.lean").write_text("", encoding="utf-8")

    direct_calls, _ = _install_run_lake_stub(monkeypatch)

    def _fake_client(*args: Any, **kwargs: Any) -> dict[str, Any]:
        raise AssertionError(
            "client_materialize_tablet_oleans must NOT be called with bwrap_role set"
        )

    monkeypatch.setattr(observations, "client_materialize_tablet_oleans", _fake_client)

    payload = observations.materialize_tablet_oleans(
        repo, ["A"], bwrap_role="lake_compiler"
    )

    assert direct_calls
    assert payload["requested_nodes"] == ["A"]


# ============================================================================
# print_axioms
# ============================================================================


def test_print_axioms_raises_when_env_unset(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Env unset + bwrap_role=None -> socket-mandatory raise; no host-lake."""
    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    direct_calls, _ = _install_run_lake_stub(monkeypatch)

    def _fake_client(*args: Any, **kwargs: Any) -> dict[str, Any]:
        raise AssertionError("client_print_axioms must NOT be called when env unset")

    monkeypatch.setattr(observations, "client_print_axioms", _fake_client)

    with pytest.raises(RuntimeError, match="checker socket required"):
        observations.print_axioms(repo, "LemmaA")

    assert direct_calls == [], "host-lake fallback must not run when socket unset"


def test_print_axioms_routes_through_client_when_env_set(
    stubbed_server: tuple[CheckerServer, list[dict[str, Any]]],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    server, direct_calls = stubbed_server
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))

    payload = observations.print_axioms(
        server.supervisor_repo,
        "LemmaA",
        timeout_secs=30.0,
    )

    assert direct_calls, "expected supervisor side to call _run_lake_command"
    assert all(c["bwrap_role"] == "lake_compiler" for c in direct_calls)
    assert payload["node"] == "LemmaA"
    assert payload["returncode"] == 0
    assert "request_id" not in payload


def test_print_axioms_response_shape_identical_in_both_modes(
    stubbed_server: tuple[CheckerServer, list[dict[str, Any]]],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    server, _direct_calls = stubbed_server

    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))
    rpc_payload = observations.print_axioms(
        server.supervisor_repo, "LemmaA", timeout_secs=30.0
    )
    rpc_keys = set(rpc_payload.keys())

    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)
    repo_direct = tmp_path / "repo_direct"
    (repo_direct / "Tablet").mkdir(parents=True)
    direct_payload = observations.print_axioms(
        repo_direct, "LemmaA", bwrap_role="lake_compiler"
    )
    direct_keys = set(direct_payload.keys())

    expected_keys = {
        "returncode",
        "stdout",
        "stderr",
        "timed_out",
        "spawn_error",
        "node",
    }
    assert expected_keys.issubset(direct_keys)
    assert expected_keys.issubset(rpc_keys)
    assert direct_keys == rpc_keys


def test_print_axioms_skips_rpc_when_bwrap_role_set(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, "/tmp/should-not-be-dialed.sock")

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    direct_calls, _ = _install_run_lake_stub(monkeypatch)

    def _fake_client(*args: Any, **kwargs: Any) -> dict[str, Any]:
        raise AssertionError("client_print_axioms must NOT be called with bwrap_role set")

    monkeypatch.setattr(observations, "client_print_axioms", _fake_client)

    payload = observations.print_axioms(repo, "LemmaA", bwrap_role="lake_compiler")

    assert direct_calls
    assert payload["node"] == "LemmaA"


# ============================================================================
# observe_lean_semantic_payloads
# ============================================================================


def test_lean_semantic_raises_when_env_unset(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Env unset + bwrap_role=None -> socket-mandatory raise; no host-lake."""
    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet" / "A.lean").write_text("", encoding="utf-8")
    (repo / "Tablet" / "B.lean").write_text("", encoding="utf-8")

    direct_calls, _ = _install_run_lake_stub(monkeypatch)

    def _fake_client(*args: Any, **kwargs: Any) -> dict[str, Any]:
        raise AssertionError(
            "client_lean_semantic_payloads must NOT be called when env unset"
        )

    monkeypatch.setattr(observations, "client_lean_semantic_payloads", _fake_client)

    with pytest.raises(RuntimeError, match="checker socket required"):
        observations.observe_lean_semantic_payloads(repo, ["A", "B"])

    assert direct_calls == [], "host-lake fallback must not run when socket unset"


def test_lean_semantic_routes_through_client_when_env_set(
    stubbed_server: tuple[CheckerServer, list[dict[str, Any]]],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    server, direct_calls = stubbed_server
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))

    payload = observations.observe_lean_semantic_payloads(
        server.supervisor_repo,
        ["A", "B"],
        timeout_secs=60.0,
    )

    assert direct_calls, "expected supervisor side to call _run_lake_command"
    assert all(c["bwrap_role"] == "lake_compiler" for c in direct_calls)

    # The wire envelope ``{"request_id": ..., "nodes": {...}}`` must be
    # unwrapped so callers see only the per-node mapping.
    assert set(payload.keys()) == {"A", "B"}
    assert "request_id" not in payload
    assert "nodes" not in payload  # the envelope's nodes key must be unwrapped
    for node_name, entry in payload.items():
        assert set(entry.keys()) == {"ok", "payload", "error"}
        assert entry["ok"] is True
        assert entry["payload"] == f"payload-of-{node_name}"


def test_lean_semantic_response_shape_identical_in_both_modes(
    stubbed_server: tuple[CheckerServer, list[dict[str, Any]]],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Both modes return the same per-node mapping (no envelope leakage)."""
    server, _direct_calls = stubbed_server

    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))
    rpc_payload = observations.observe_lean_semantic_payloads(
        server.supervisor_repo, ["A", "B"], timeout_secs=60.0
    )

    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)
    repo_direct = tmp_path / "repo_direct"
    (repo_direct / "Tablet").mkdir(parents=True)
    (repo_direct / "Tablet" / "A.lean").write_text("", encoding="utf-8")
    (repo_direct / "Tablet" / "B.lean").write_text("", encoding="utf-8")
    direct_payload = observations.observe_lean_semantic_payloads(
        repo_direct, ["A", "B"], bwrap_role="lake_compiler"
    )

    assert set(direct_payload.keys()) == set(rpc_payload.keys()) == {"A", "B"}
    for node_name in direct_payload:
        assert (
            set(direct_payload[node_name].keys())
            == set(rpc_payload[node_name].keys())
            == {"ok", "payload", "error"}
        )


def test_lean_semantic_skips_rpc_when_bwrap_role_set(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, "/tmp/should-not-be-dialed.sock")

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet" / "A.lean").write_text("", encoding="utf-8")

    direct_calls, _ = _install_run_lake_stub(monkeypatch)

    def _fake_client(*args: Any, **kwargs: Any) -> dict[str, Any]:
        raise AssertionError(
            "client_lean_semantic_payloads must NOT be called with bwrap_role set"
        )

    monkeypatch.setattr(observations, "client_lean_semantic_payloads", _fake_client)

    payload = observations.observe_lean_semantic_payloads(
        repo, ["A"], bwrap_role="lake_compiler"
    )

    assert direct_calls
    assert set(payload.keys()) == {"A"}


def test_lean_semantic_unwrap_returns_plain_dict_of_dicts(
    stubbed_server: tuple[CheckerServer, list[dict[str, Any]]],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Specific unwrap test: the routed return value must be a plain
    dict-of-dicts (matching the direct-lake path), not a Mapping or
    something carrying the wire envelope keys.
    """
    server, _direct_calls = stubbed_server
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))

    payload = observations.observe_lean_semantic_payloads(
        server.supervisor_repo, ["A"], timeout_secs=30.0
    )

    # The outer mapping is a plain dict and so is each per-node entry.
    assert type(payload) is dict
    assert type(payload["A"]) is dict
    # Direct-lake's signature is Dict[str, Dict[str, Any]] — match it.
    assert "request_id" not in payload
    assert "nodes" not in payload


def test_lean_semantic_supervisor_unavailable_propagates(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Missing socket -> CheckerRpcError, no silent fall-through."""
    bogus_socket = tmp_path / "missing_dir" / "checker.sock"
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(bogus_socket))

    direct_calls, _ = _install_run_lake_stub(monkeypatch)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet" / "A.lean").write_text("", encoding="utf-8")

    with pytest.raises(CheckerRpcError) as excinfo:
        observations.observe_lean_semantic_payloads(repo, ["A"], timeout_secs=2.0)

    assert excinfo.value.kind == "supervisor_unavailable"
    assert direct_calls == []


# ============================================================================
# build_tablet
# ============================================================================


def test_build_tablet_raises_when_env_unset(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Env unset + bwrap_role=None -> socket-mandatory raise; no host-lake."""
    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    direct_calls, _ = _install_run_lake_stub(monkeypatch)

    def _fake_client(*args: Any, **kwargs: Any) -> dict[str, Any]:
        raise AssertionError("client_build_tablet must NOT be called when env unset")

    monkeypatch.setattr(observations, "client_build_tablet", _fake_client)

    with pytest.raises(RuntimeError, match="checker socket required"):
        observations.build_tablet(repo)

    assert direct_calls == [], "host-lake fallback must not run when socket unset"


def test_build_tablet_routes_through_client_when_env_set(
    stubbed_server: tuple[CheckerServer, list[dict[str, Any]]],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    server, direct_calls = stubbed_server
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))

    payload = observations.build_tablet(server.supervisor_repo, timeout_secs=30.0)

    assert direct_calls, "expected supervisor side to call _run_lake_command"
    assert all(c["bwrap_role"] == "lake_compiler" for c in direct_calls)
    assert any(c["args"] == ["build", "Tablet"] for c in direct_calls)
    assert payload["returncode"] == 0
    assert "request_id" not in payload


def test_build_tablet_response_shape_identical_in_both_modes(
    stubbed_server: tuple[CheckerServer, list[dict[str, Any]]],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    server, _direct_calls = stubbed_server

    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))
    rpc_payload = observations.build_tablet(server.supervisor_repo, timeout_secs=30.0)
    rpc_keys = set(rpc_payload.keys())

    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)
    repo_direct = tmp_path / "repo_direct"
    (repo_direct / "Tablet").mkdir(parents=True)
    direct_payload = observations.build_tablet(repo_direct, bwrap_role="lake_compiler")
    direct_keys = set(direct_payload.keys())

    expected_keys = {"returncode", "stdout", "stderr", "timed_out", "spawn_error"}
    assert expected_keys.issubset(direct_keys)
    assert expected_keys.issubset(rpc_keys)
    assert direct_keys == rpc_keys


def test_build_tablet_skips_rpc_when_bwrap_role_set(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, "/tmp/should-not-be-dialed.sock")

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    direct_calls, _ = _install_run_lake_stub(monkeypatch)

    def _fake_client(*args: Any, **kwargs: Any) -> dict[str, Any]:
        raise AssertionError("client_build_tablet must NOT be called with bwrap_role set")

    monkeypatch.setattr(observations, "client_build_tablet", _fake_client)

    payload = observations.build_tablet(repo, bwrap_role="lake_compiler")

    assert direct_calls
    assert payload["returncode"] == 0


# ============================================================================
# prepare_compiled_support
# ============================================================================


def test_prepare_compiled_support_raises_when_env_unset(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Env unset + bwrap_role=None -> socket-mandatory raise; no host-lake."""
    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    direct_calls, _ = _install_run_lake_stub(monkeypatch)

    def _fake_client(*args: Any, **kwargs: Any) -> dict[str, Any]:
        raise AssertionError(
            "client_prepare_compiled_support must NOT be called when env unset"
        )

    monkeypatch.setattr(observations, "client_prepare_compiled_support", _fake_client)

    with pytest.raises(RuntimeError, match="checker socket required"):
        observations.prepare_compiled_support(repo)

    assert direct_calls == [], "host-lake fallback must not run when socket unset"


def test_prepare_compiled_support_routes_through_client_when_env_set(
    stubbed_server: tuple[CheckerServer, list[dict[str, Any]]],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    server, direct_calls = stubbed_server
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))

    payload = observations.prepare_compiled_support(
        server.supervisor_repo, timeout_secs=30.0
    )

    assert direct_calls, "expected supervisor side to call _run_lake_command"
    assert all(c["bwrap_role"] == "lake_compiler" for c in direct_calls)
    assert any(c["args"] == ["exe", "cache", "get"] for c in direct_calls)
    assert payload["returncode"] == 0
    assert payload["steps_completed"] == ["cache_get"]
    assert "request_id" not in payload


def test_prepare_compiled_support_response_shape_identical_in_both_modes(
    stubbed_server: tuple[CheckerServer, list[dict[str, Any]]],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    server, _direct_calls = stubbed_server

    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(server.socket_path))
    rpc_payload = observations.prepare_compiled_support(
        server.supervisor_repo, timeout_secs=30.0
    )
    rpc_keys = set(rpc_payload.keys())

    monkeypatch.delenv(TRELLIS_CHECKER_SOCKET_ENV, raising=False)
    repo_direct = tmp_path / "repo_direct"
    (repo_direct / "Tablet").mkdir(parents=True)
    direct_payload = observations.prepare_compiled_support(
        repo_direct, bwrap_role="lake_compiler"
    )
    direct_keys = set(direct_payload.keys())

    expected_keys = {
        "steps_completed",
        "returncode",
        "stdout",
        "stderr",
        "timed_out",
        "spawn_error",
    }
    assert expected_keys.issubset(direct_keys)
    assert expected_keys.issubset(rpc_keys)
    assert direct_keys == rpc_keys


def test_prepare_compiled_support_skips_rpc_when_bwrap_role_set(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, "/tmp/should-not-be-dialed.sock")

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)

    direct_calls, _ = _install_run_lake_stub(monkeypatch)

    def _fake_client(*args: Any, **kwargs: Any) -> dict[str, Any]:
        raise AssertionError(
            "client_prepare_compiled_support must NOT be called with bwrap_role set"
        )

    monkeypatch.setattr(observations, "client_prepare_compiled_support", _fake_client)

    payload = observations.prepare_compiled_support(repo, bwrap_role="lake_compiler")

    assert direct_calls
    assert payload["steps_completed"] == ["cache_get"]


def test_lake_compiler_bwrap_prepends_elan_bin_to_path(tmp_path, monkeypatch):
    """The lake_compiler bwrap resolves the OUTER `lake` via the server process's
    PATH (wrap_command adds no --setenv PATH). Prepend the resolved elan bin so
    `lake` resolves even if the checker server was launched from a shell without
    elan on PATH — mirroring the worker side. bwrap_role=None must not touch PATH."""
    repo = tmp_path / "repo"
    repo.mkdir()

    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.setattr(
        "trellis.sandbox.wrap_command", lambda inner, **kw: ["bwrap", "--stub", *inner]
    )
    monkeypatch.setattr(
        "trellis.host_runtime.worker_elan_home", lambda: Path("/opt/fake-elan")
    )
    monkeypatch.setenv("PATH", "/usr/bin:/bin")  # no elan on PATH (the failure case)

    captured: dict[str, Any] = {}

    def _fake_run(argv, **kw):
        captured["env"] = kw.get("env")

        class _R:
            returncode = 0
            stdout = ""
            stderr = ""

        return _R()

    monkeypatch.setattr(observations.subprocess, "run", _fake_run)

    observations._run_lake_command(
        repo, ["build"], timeout_secs=10, bwrap_role="lake_compiler"
    )
    path_entries = captured["env"]["PATH"].split(os.pathsep)
    assert path_entries[0] == str(Path("/opt/fake-elan") / "bin"), captured["env"]["PATH"]

    captured.clear()
    observations._run_lake_command(repo, ["build"], timeout_secs=10, bwrap_role=None)
    assert str(Path("/opt/fake-elan") / "bin") not in captured["env"]["PATH"].split(os.pathsep)
