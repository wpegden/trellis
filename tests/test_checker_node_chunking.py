"""Byte-driven chunking of the checker RPC ``nodes`` list.

The acceptance path materializes every present Tablet node in one RPC. At
1,023 present nodes a live run hit ``MAX_NODES_PER_REQUEST`` and
halted: a six-node decomposition asked for 1,029 and the client-side
validator rejected the request, so no node could be added. Both wire
constants stay fixed; the node list is split instead.

Coverage:
  - the split rule (byte bound, count bound, order, empty input)
  - the 1,029-node case that halted the run
  - merge semantics for ``materialize_oleans`` and ``lean_semantic_payloads``
  - the CLI reporting ``CheckerRpcError`` as JSON instead of letting it escape
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Dict, Sequence

import pytest

import trellis.atomic_actions.cli as cli
import trellis.atomic_actions.observations as observations
from trellis.atomic_actions.checker_client import (
    CheckerRpcError,
    TRELLIS_CHECKER_SOCKET_ENV,
    _validate_nodes,
)
from trellis.checker.protocol import (
    MAX_LINE_BYTES,
    MAX_NODES_PER_REQUEST,
    NODE_NAME_MAX_LEN,
)


def _names(count: int, *, length: int = 22) -> list[str]:
    """``count`` distinct legal node names of exactly ``length`` chars."""
    out = []
    for idx in range(count):
        suffix = str(idx)
        out.append("N" + "x" * (length - 1 - len(suffix)) + suffix)
    return out


class _FakeMaterializeClient:
    """Records each chunk and replies with a canned per-chunk payload."""

    def __init__(self, responses: Sequence[Dict[str, Any]] | None = None) -> None:
        self.chunks: list[list[str]] = []
        self.responses = list(responses or [])

    def __call__(
        self,
        socket_path: Any,
        repo: Any,
        nodes: Any,
        *,
        timeout_secs: float,
    ) -> Dict[str, Any]:
        chunk = list(nodes)
        # The real client validates before sending; a chunk that would be
        # rejected on the wire must fail here too.
        _validate_nodes(chunk)
        self.chunks.append(chunk)
        if self.responses:
            return dict(self.responses[len(self.chunks) - 1])
        return {
            "request_id": len(self.chunks),
            "requested_nodes": chunk,
            "materialized_nodes": chunk,
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }


# ============================================================================
# split rule
# ============================================================================


def test_chunking_preserves_order_and_drops_nothing() -> None:
    names = _names(3000)
    chunks = observations._chunk_node_names(names)
    assert [name for chunk in chunks for name in chunk] == names


def test_chunking_respects_the_node_count_cap() -> None:
    # Short names keep the byte bound slack, so the count bound is the one
    # that fires.
    names = _names(2500, length=4)
    chunks = observations._chunk_node_names(names)
    assert all(len(chunk) <= MAX_NODES_PER_REQUEST for chunk in chunks)
    assert len(chunks[0]) == MAX_NODES_PER_REQUEST


def test_chunking_respects_the_byte_cap_with_maximal_names() -> None:
    names = [("A" * (NODE_NAME_MAX_LEN - 4)) + f"{idx:04d}" for idx in range(600)]
    assert all(len(name) == NODE_NAME_MAX_LEN for name in names)

    chunks = observations._chunk_node_names(names)

    assert all(len(chunk) < MAX_NODES_PER_REQUEST for chunk in chunks)
    for chunk in chunks:
        encoded = sum(len(name.encode("utf-8")) + 3 for name in chunk)
        assert encoded <= observations.CHECKER_NODE_CHUNK_MAX_BYTES
        line = json.dumps(
            {
                "op": "materialize_oleans",
                "request_id": 1,
                "nodes": chunk,
                "timeout_secs": 3600.0,
            }
        ).encode("utf-8")
        assert len(line) < MAX_LINE_BYTES


def test_chunking_of_empty_list_still_sends_one_request() -> None:
    # An empty ``nodes`` list means "every present node" to the server, so
    # it must reach the wire rather than being optimised away.
    assert observations._chunk_node_names([]) == [[]]


def test_chunk_boundary_neither_drops_nor_duplicates() -> None:
    names = _names(2049, length=4)
    chunks = observations._chunk_node_names(names)
    flat = [name for chunk in chunks for name in chunk]
    assert len(flat) == len(names)
    assert len(set(flat)) == len(set(names))
    assert flat == names


def test_the_1029_node_case_that_halted_the_run() -> None:
    """1,023 present nodes plus a six-node decomposition."""
    names = _names(1029)
    chunks = observations._chunk_node_names(names)

    assert len(chunks) == 2
    assert sum(len(chunk) for chunk in chunks) == 1029
    for chunk in chunks:
        # No validation error: this is exactly what the run could not do.
        _validate_nodes(chunk)


# ============================================================================
# materialize_oleans merge
# ============================================================================


@pytest.fixture()
def socket_env(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setenv(TRELLIS_CHECKER_SOCKET_ENV, str(tmp_path / "checker.sock"))


def test_materialize_chunked_matches_single_call(
    socket_env: None,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    # Long names so the byte bound forces several chunks while the list
    # stays short enough for the single-call baseline to be wire-legal.
    names = _names(800, length=NODE_NAME_MAX_LEN)

    chunked = _FakeMaterializeClient()
    monkeypatch.setattr(observations, "client_materialize_tablet_oleans", chunked)
    chunked_payload = observations.materialize_tablet_oleans(tmp_path, names)

    single = _FakeMaterializeClient()
    monkeypatch.setattr(observations, "client_materialize_tablet_oleans", single)
    monkeypatch.setattr(
        observations, "_chunk_node_names", lambda node_names: [list(node_names)]
    )
    single_payload = observations.materialize_tablet_oleans(tmp_path, names)

    assert len(chunked.chunks) > 1
    assert len(single.chunks) == 1
    assert chunked_payload == single_payload
    assert chunked_payload["requested_nodes"] == names
    assert chunked_payload["materialized_nodes"] == names
    assert "request_id" not in chunked_payload


def test_materialize_merge_takes_first_non_zero_returncode(
    socket_env: None,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    names = _names(1029)
    responses = [
        {
            "requested_nodes": [],
            "materialized_nodes": [],
            "returncode": 0,
            "stdout": "first-out",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        },
        {
            "requested_nodes": [],
            "materialized_nodes": [],
            "returncode": 3,
            "stdout": "second-out",
            "stderr": "boom",
            "timed_out": False,
            "spawn_error": "",
        },
    ]
    fake = _FakeMaterializeClient(responses)
    monkeypatch.setattr(observations, "client_materialize_tablet_oleans", fake)

    payload = observations.materialize_tablet_oleans(tmp_path, names)

    assert payload["returncode"] == 3
    assert payload["stdout"] == "first-outsecond-out"
    assert payload["stderr"] == "boom"


def test_materialize_merge_propagates_timeout_and_first_spawn_error(
    socket_env: None,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    names = _names(1029)
    base = {
        "requested_nodes": [],
        "materialized_nodes": [],
        "returncode": 0,
        "stdout": "",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }
    responses = [
        dict(base),
        {**base, "timed_out": True, "spawn_error": "no such binary"},
    ]
    fake = _FakeMaterializeClient(responses)
    monkeypatch.setattr(observations, "client_materialize_tablet_oleans", fake)

    payload = observations.materialize_tablet_oleans(tmp_path, names)

    assert payload["timed_out"] is True
    assert payload["spawn_error"] == "no such binary"


def test_materialize_merge_unions_materialized_nodes_first_seen(
    socket_env: None,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    names = _names(1029)
    base = {
        "requested_nodes": [],
        "returncode": 0,
        "stdout": "",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }
    responses = [
        {**base, "materialized_nodes": ["Preamble", "A", "B"]},
        {**base, "materialized_nodes": ["Preamble", "C", "A"]},
    ]
    fake = _FakeMaterializeClient(responses)
    monkeypatch.setattr(observations, "client_materialize_tablet_oleans", fake)

    payload = observations.materialize_tablet_oleans(tmp_path, names)

    assert payload["materialized_nodes"] == ["Preamble", "A", "B", "C"]


def test_materialize_single_chunk_keeps_response_key_set(
    socket_env: None,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    fake = _FakeMaterializeClient()
    monkeypatch.setattr(observations, "client_materialize_tablet_oleans", fake)

    payload = observations.materialize_tablet_oleans(tmp_path, ["A", "B"])

    assert set(payload) == {
        "requested_nodes",
        "materialized_nodes",
        "returncode",
        "stdout",
        "stderr",
        "timed_out",
        "spawn_error",
    }
    assert fake.chunks == [["A", "B"]]


# ============================================================================
# lean_semantic_payloads merge
# ============================================================================


def test_semantic_payloads_chunked_matches_single_call(
    socket_env: None,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    names = _names(1029)
    seen_chunks: list[list[str]] = []

    def _fake_client(
        socket_path: Any,
        repo: Any,
        nodes: Any,
        *,
        timeout_secs: float,
    ) -> Dict[str, Dict[str, Any]]:
        chunk = list(nodes)
        _validate_nodes(chunk)
        seen_chunks.append(chunk)
        return {
            name: {"ok": True, "payload": f"p-{name}", "error": ""} for name in chunk
        }

    monkeypatch.setattr(observations, "client_lean_semantic_payloads", _fake_client)
    chunked = observations.observe_lean_semantic_payloads(tmp_path, names)

    assert len(seen_chunks) == 2
    assert set(chunked) == set(names)
    assert list(chunked) == names
    assert all(entry["payload"] == f"p-{name}" for name, entry in chunked.items())
    assert all(type(entry) is dict for entry in chunked.values())


# ============================================================================
# CLI error surfacing
# ============================================================================


def test_cli_materialize_reports_rpc_error_as_json(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    tmp_path: Path,
) -> None:
    def _boom(*args: Any, **kwargs: Any) -> Dict[str, Any]:
        raise CheckerRpcError("invalid_request", "nodes list of 1029 exceeds cap 1024")

    monkeypatch.setattr(cli, "materialize_tablet_oleans", _boom)

    exit_code = cli.main(["materialize-tablet-oleans", str(tmp_path), "--node", "A"])

    assert exit_code == 2
    payload = json.loads(capsys.readouterr().out)
    assert payload["error"] == "invalid_request: nodes list of 1029 exceeds cap 1024"


def test_cli_semantic_payloads_reports_rpc_error_as_json(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    tmp_path: Path,
) -> None:
    def _boom(*args: Any, **kwargs: Any) -> Dict[str, Any]:
        raise CheckerRpcError("supervisor_unavailable", "connection refused")

    monkeypatch.setattr(cli, "observe_lean_semantic_payloads", _boom)

    exit_code = cli.main(["lean-semantic-payloads", str(tmp_path), "--node", "A"])

    assert exit_code == 2
    payload = json.loads(capsys.readouterr().out)
    assert payload["error"] == "supervisor_unavailable: connection refused"
