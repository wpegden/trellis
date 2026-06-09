"""Worker-side RPC client for the unified-checker UNIX-socket dispatcher.

Step 2 of the migration plan (design plan §5): the typed client module.
Mirrors the public observation surface in :mod:`trellis.atomic_actions.observations`
so each ``client_*`` function returns the same dict shape its observation
counterpart returns today. Step 3 will wire these into the observation
functions behind a ``TRELLIS_CHECKER_SOCKET`` env-var flag.

Connection lifecycle
--------------------
**Per-call connection** (design plan §5: "Recommend per-call for v1"). Each
method opens a fresh ``AF_UNIX`` ``SOCK_STREAM``, sends one
newline-terminated JSON request, reads one newline-terminated JSON
response, closes. Caching is a future optimization.

Validation
----------
Client-side parameter validation **mirrors** the server's
(:mod:`trellis.checker.protocol`) so a malformed request is rejected
locally before it hits the wire. The server still re-validates: this is
defence in depth, not a removal of server-side enforcement.

Error model
-----------
- Server returns an ``rpc_error`` envelope -> :class:`CheckerRpcError`
  with ``kind`` and ``message`` echoed from the server.
- ``ConnectionRefusedError`` / socket timeout / ``ECONNRESET`` ->
  :class:`CheckerRpcError` with ``kind="supervisor_unavailable"``.
- Garbage on the wire (non-UTF-8, non-JSON, non-object,
  ``request_id`` mismatch) -> :class:`CheckerRpcError` with
  ``kind="malformed_response"``.
- A successful business response with ``returncode != 0`` (e.g. lake
  failed to build) is surfaced **inside** the response payload and is
  NOT raised — that mirrors the observation functions today, which
  return non-zero ``returncode`` in their dicts rather than raising.

Configuration helper
--------------------
:func:`_resolve_socket_path` reads the ``TRELLIS_CHECKER_SOCKET``
environment variable and returns a :class:`Path` if it is set and
non-empty, ``None`` otherwise. Callers can use this to route via the
client only when the supervisor has explicitly forwarded the socket
path; absent env var -> caller falls through to direct lake. No
default fallback is provided (the supervisor controls the path; the
worker must be told).
"""

from __future__ import annotations

import errno
import itertools
import json
import math
import os
import socket
import threading
from pathlib import Path
from typing import Any, Dict, Mapping, Optional, Sequence

from trellis.checker.protocol import (
    MAX_LINE_BYTES,
    MAX_MESSAGE_BYTES,
    MAX_NODES_PER_REQUEST,
    NODE_NAME_MAX_LEN,
    NODE_NAME_REGEX,
    NODE_NAME_REGEX_STR,
    TIMEOUT_SECS_DEFAULT,
    TIMEOUT_SECS_MAX,
    TIMEOUT_SECS_MIN,
)


# Env-var contract: when set+non-empty by the supervisor, caller-side code
# may route observation calls via this client. When unset, fall through to
# direct lake. No default value — only the supervisor knows the path.
TRELLIS_CHECKER_SOCKET_ENV = "TRELLIS_CHECKER_SOCKET"

# Phase 2 of the bwrap-only migration: per-burst HMAC token minted by the
# bridge at dispatch time and forwarded into the burst via `--setenv`
# (see ``trellis/sandbox.py::_PASSTHROUGH_VALUE_ENV_VARS``). When set,
# the client embeds the token in every request envelope under
# ``auth_token``; the server's per-request gate validates it against
# the live registry. Absence is tolerated end-to-end (the server's gate
# returns True when its registry is empty, the dormant-Phase-2 path).
TRELLIS_CHECKER_TOKEN_ENV = "TRELLIS_CHECKER_TOKEN"

# Default ping timeout. Ping is purely a control-plane health-check; it
# never touches lake, so a small budget is appropriate.
DEFAULT_PING_TIMEOUT_SECS = 10.0

# Per-request id counter. Thread-safe via the GIL for the increment but we
# wrap with a lock to avoid relying on that subtlety; cost is negligible.
_request_id_counter = itertools.count(1)
_request_id_lock = threading.Lock()


class CheckerRpcError(Exception):
    """Transport- or protocol-level RPC failure surfaced to the caller.

    Distinguished from a *successful* RPC whose payload happens to carry
    a non-zero ``returncode`` (the latter is returned in the response
    dict, not raised).

    Carries a stable ``kind`` string so callers can branch on it:

    - ``invalid_request``: client-local validation rejected the call
      before sending. The server's regex/length/clamps must match this
      so a request that would have been rejected server-side is also
      rejected here.
    - ``supervisor_unavailable``: the socket is missing, the connect
      failed, the server crashed mid-request, or the read/write timed
      out. Caller may treat this like a transport failure (today's
      ``spawn_error`` path in the observation functions).
    - ``malformed_response``: the server wrote bytes that aren't a
      valid JSON object, or the ``request_id`` echoed back doesn't
      match what we sent. Should never happen in production.
    - Any kind echoed from the server's ``rpc_error`` envelope (e.g.
      ``malformed_request``, ``unknown_op``, ``sync_failed``,
      ``internal_error``) is propagated verbatim.
    """

    def __init__(self, kind: str, message: str) -> None:
        super().__init__(f"{kind}: {message}")
        self.kind = kind
        self.message = message


# ---------------------------- configuration ----------------------------


def _resolve_socket_path() -> Optional[Path]:
    """Return the configured checker socket path, or ``None`` if unset.

    Reads ``TRELLIS_CHECKER_SOCKET`` from the process environment.
    Strips whitespace; an empty/whitespace-only value is treated as
    unset. No default fallback — when ``None``, callers should route
    via direct lake.
    """
    raw = os.environ.get(TRELLIS_CHECKER_SOCKET_ENV, "")
    stripped = raw.strip()
    if not stripped:
        return None
    return Path(stripped)


# ---------------------------- validation ----------------------------


def _validate_node_name(value: Any, *, field_name: str = "node_name") -> str:
    """Mirror :func:`trellis.checker.protocol._validate_node_name`.

    Raises :class:`CheckerRpcError` (kind ``invalid_request``) on
    failure rather than ``ProtocolError`` so callers get a single
    exception type to catch.
    """
    if not isinstance(value, str):
        raise CheckerRpcError(
            "invalid_request",
            f"{field_name} must be a string, got {type(value).__name__}",
        )
    if len(value) == 0:
        raise CheckerRpcError("invalid_request", f"{field_name} is empty")
    if len(value) > NODE_NAME_MAX_LEN:
        raise CheckerRpcError(
            "invalid_request",
            f"{field_name} exceeds {NODE_NAME_MAX_LEN} chars (got {len(value)})",
        )
    if NODE_NAME_REGEX.fullmatch(value) is None:
        raise CheckerRpcError(
            "invalid_request",
            f"{field_name} does not match {NODE_NAME_REGEX_STR}: {value!r}",
        )
    return value


def _validate_nodes(value: Any) -> tuple[str, ...]:
    if not isinstance(value, (list, tuple)):
        raise CheckerRpcError(
            "invalid_request",
            f"nodes must be a list, got {type(value).__name__}",
        )
    if len(value) > MAX_NODES_PER_REQUEST:
        raise CheckerRpcError(
            "invalid_request",
            f"nodes list of {len(value)} exceeds cap {MAX_NODES_PER_REQUEST}",
        )
    out: list[str] = []
    for idx, raw in enumerate(value):
        out.append(_validate_node_name(raw, field_name=f"nodes[{idx}]"))
    return tuple(out)


def _validate_timeout(value: Any) -> float:
    """Mirror the server's ``_coerce_timeout``.

    The server *clamps* out-of-band but finite values; we do the same.
    Non-finite (``nan``, ``inf``) and non-numeric inputs are hard
    failures here so a buggy caller fails locally instead of negotiating
    with the wire.
    """
    if value is None:
        # The server's ``_coerce_timeout(None)`` returns ``TIMEOUT_SECS_DEFAULT``;
        # mirror that so callers can pass ``None`` to mean "default".
        return TIMEOUT_SECS_DEFAULT
    try:
        coerced = float(value)
    except (TypeError, ValueError):
        raise CheckerRpcError(
            "invalid_request", f"timeout_secs is not a number: {value!r}"
        )
    if not math.isfinite(coerced):
        raise CheckerRpcError(
            "invalid_request", f"timeout_secs must be finite, got {value!r}"
        )
    if coerced < TIMEOUT_SECS_MIN:
        return TIMEOUT_SECS_MIN
    if coerced > TIMEOUT_SECS_MAX:
        return TIMEOUT_SECS_MAX
    return coerced


def _validate_socket_path(socket_path: Any) -> Path:
    if isinstance(socket_path, Path):
        candidate = socket_path
    elif isinstance(socket_path, str):
        if not socket_path.strip():
            raise CheckerRpcError(
                "invalid_request", "socket_path is empty"
            )
        candidate = Path(socket_path)
    else:
        raise CheckerRpcError(
            "invalid_request",
            f"socket_path must be a Path or str, got {type(socket_path).__name__}",
        )
    return candidate


def _next_request_id() -> int:
    with _request_id_lock:
        return next(_request_id_counter)


# ---------------------------- transport ----------------------------


def _read_one_line(sock: socket.socket, *, deadline_secs: float) -> bytes:
    """Read up to one ``\\n``-terminated frame from ``sock``.

    Enforces the wire-level ``MAX_MESSAGE_BYTES`` cap. Returns the line
    *without* the trailing newline. Raises :class:`CheckerRpcError` on
    transport failure.
    """
    buffer = bytearray()
    sock.settimeout(deadline_secs)
    while True:
        idx = buffer.find(b"\n")
        if idx >= 0:
            return bytes(buffer[:idx])
        if len(buffer) > MAX_MESSAGE_BYTES:
            raise CheckerRpcError(
                "malformed_response",
                f"response exceeded {MAX_MESSAGE_BYTES} bytes before newline",
            )
        try:
            chunk = sock.recv(8192)
        except socket.timeout:
            raise CheckerRpcError(
                "supervisor_unavailable",
                f"timed out waiting for response after {deadline_secs}s",
            )
        except (ConnectionResetError, BrokenPipeError) as exc:
            raise CheckerRpcError(
                "supervisor_unavailable",
                f"connection reset while reading response: {exc}",
            )
        except OSError as exc:
            raise CheckerRpcError(
                "supervisor_unavailable",
                f"recv failed: {exc}",
            )
        if not chunk:
            # Clean EOF before terminator: server closed the socket
            # without sending the rest of the frame.
            if buffer:
                raise CheckerRpcError(
                    "supervisor_unavailable",
                    "server closed connection before terminator",
                )
            raise CheckerRpcError(
                "supervisor_unavailable",
                "server closed connection without writing a response",
            )
        buffer.extend(chunk)


def _connect(socket_path: Path, *, connect_timeout_secs: float) -> socket.socket:
    """Open a fresh ``AF_UNIX`` ``SOCK_STREAM`` to ``socket_path``."""
    if not isinstance(socket_path, Path):
        raise CheckerRpcError(
            "invalid_request",
            f"socket_path must be a Path, got {type(socket_path).__name__}",
        )
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(connect_timeout_secs)
    try:
        sock.connect(str(socket_path))
    except FileNotFoundError as exc:
        with _suppress_close(sock):
            sock.close()
        raise CheckerRpcError(
            "supervisor_unavailable",
            f"checker socket not found at {socket_path}: {exc}",
        )
    except ConnectionRefusedError as exc:
        with _suppress_close(sock):
            sock.close()
        raise CheckerRpcError(
            "supervisor_unavailable",
            f"checker server refused connection at {socket_path}: {exc}",
        )
    except PermissionError as exc:
        with _suppress_close(sock):
            sock.close()
        raise CheckerRpcError(
            "supervisor_unavailable",
            f"permission denied connecting to {socket_path}: {exc}",
        )
    except socket.timeout:
        with _suppress_close(sock):
            sock.close()
        raise CheckerRpcError(
            "supervisor_unavailable",
            f"timed out connecting to {socket_path} after {connect_timeout_secs}s",
        )
    except OSError as exc:
        with _suppress_close(sock):
            sock.close()
        # EAGAIN/EWOULDBLOCK on connect maps to "supervisor not ready".
        if exc.errno in (errno.EAGAIN, errno.EWOULDBLOCK, errno.ENOENT, errno.ECONNREFUSED):
            raise CheckerRpcError(
                "supervisor_unavailable",
                f"could not reach checker at {socket_path}: {exc}",
            )
        raise CheckerRpcError(
            "supervisor_unavailable",
            f"connect to {socket_path} failed: {exc}",
        )
    return sock


class _suppress_close:
    """Context manager that swallows ``OSError`` during ``sock.close()``.

    Used so the connect-error paths above can close the socket without
    masking the real failure with a follow-on close exception.
    """

    def __init__(self, sock: socket.socket) -> None:
        self._sock = sock

    def __enter__(self) -> "_suppress_close":
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> bool:
        return True  # swallow any close-path exception


def _send_request(
    sock: socket.socket,
    payload: Mapping[str, Any],
    *,
    deadline_secs: float,
) -> None:
    encoded = json.dumps(payload, separators=(",", ":"), ensure_ascii=False).encode("utf-8") + b"\n"
    if len(encoded) > MAX_LINE_BYTES:
        raise CheckerRpcError(
            "invalid_request",
            f"request line of {len(encoded)} bytes exceeds {MAX_LINE_BYTES}",
        )
    sock.settimeout(deadline_secs)
    try:
        sock.sendall(encoded)
    except (ConnectionResetError, BrokenPipeError) as exc:
        raise CheckerRpcError(
            "supervisor_unavailable",
            f"connection reset while sending request: {exc}",
        )
    except socket.timeout:
        raise CheckerRpcError(
            "supervisor_unavailable",
            f"timed out sending request after {deadline_secs}s",
        )
    except OSError as exc:
        raise CheckerRpcError(
            "supervisor_unavailable",
            f"sendall failed: {exc}",
        )


def _decode_response(line: bytes) -> Mapping[str, Any]:
    try:
        text = line.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise CheckerRpcError(
            "malformed_response", f"response is not valid UTF-8: {exc}"
        )
    try:
        decoded = json.loads(text)
    except json.JSONDecodeError as exc:
        raise CheckerRpcError(
            "malformed_response", f"response is not valid JSON: {exc.msg}"
        )
    if not isinstance(decoded, dict):
        raise CheckerRpcError(
            "malformed_response",
            f"response must be a JSON object, got {type(decoded).__name__}",
        )
    return decoded


def _check_response_envelope(
    response: Mapping[str, Any], *, expected_request_id: int
) -> Mapping[str, Any]:
    """Inspect a decoded response and surface ``rpc_error`` envelopes.

    The server's protocol rule is "response always echoes ``request_id``".
    A response that omits ``request_id`` or echoes the wrong value
    indicates wire corruption or a misbehaving server; treat it as
    malformed.
    """
    if "rpc_error" in response:
        rpc_err = response.get("rpc_error", {})
        if isinstance(rpc_err, dict):
            kind = str(rpc_err.get("kind") or "internal_error")
            message = str(rpc_err.get("message") or "unspecified rpc error")
        else:
            kind = "internal_error"
            message = str(rpc_err)
        raise CheckerRpcError(kind, message)

    rid = response.get("request_id")
    if not isinstance(rid, int) or isinstance(rid, bool):
        raise CheckerRpcError(
            "malformed_response",
            f"response missing integer request_id: {rid!r}",
        )
    if rid != expected_request_id:
        raise CheckerRpcError(
            "malformed_response",
            f"response request_id {rid} != expected {expected_request_id}",
        )
    return response


def _maybe_inject_auth_token(request: Mapping[str, Any]) -> Mapping[str, Any]:
    """Return a copy of ``request`` with ``auth_token`` from the env
    appended when the env var is set+non-empty. Returns ``request``
    unchanged when the env var is unset.

    Phase 2 of the bwrap-only migration: this is the SOLE injection
    point so every op picks up the token uniformly. The server tolerates
    missing tokens when its registry is empty (dormant case); a present
    token is required only post-restart, when the new server-side gate
    is loaded. Either way the client sends the token whenever the env
    var carries one — no decision logic needed here.
    """
    token = os.environ.get(TRELLIS_CHECKER_TOKEN_ENV, "")
    if not token or not token.strip():
        return request
    annotated = dict(request)
    annotated["auth_token"] = token.strip()
    return annotated


def _round_trip(
    socket_path: Path,
    request: Mapping[str, Any],
    *,
    timeout_secs: float,
) -> Mapping[str, Any]:
    """Open a fresh connection, send one request, return the parsed response.

    ``timeout_secs`` is the *call-level* deadline (per RPC). We treat it
    as the budget for both connect and read; in practice connects are
    sub-millisecond and reads dominate. We give ``connect`` a short fixed
    budget (``min(timeout_secs, 10s)``) so a missing socket is detected
    quickly rather than waiting the full lake-build timeout for a
    connection error.
    """
    connect_budget = min(timeout_secs, 10.0)
    # Add 5s slack to the read deadline so the server has a moment to
    # write the response after lake exits, even on a near-timeout op.
    read_deadline = max(timeout_secs + 5.0, connect_budget)

    # Phase 2 (bwrap-only migration): augment the outgoing request with
    # the per-burst auth token when forwarded via env. Single injection
    # point for all ops; see _maybe_inject_auth_token for the contract.
    request = _maybe_inject_auth_token(request)

    sock = _connect(socket_path, connect_timeout_secs=connect_budget)
    try:
        _send_request(sock, request, deadline_secs=read_deadline)
        line = _read_one_line(sock, deadline_secs=read_deadline)
    finally:
        try:
            sock.close()
        except OSError:
            pass

    response = _decode_response(line)
    expected_id = int(request.get("request_id", -1))
    return _check_response_envelope(response, expected_request_id=expected_id)


# ---------------------------- public client API ----------------------------


def client_ping(
    socket_path: Any,
    *,
    timeout_secs: float = DEFAULT_PING_TIMEOUT_SECS,
) -> Mapping[str, Any]:
    """Health-check round-trip. Returns the server's pong dict.

    Maps directly to the server's ``ping`` op (see
    :meth:`trellis.checker.server.CheckerServer._op_ping`); response
    has ``pong: True`` on success plus ``server_pid``, ``uptime_secs``,
    ``supervisor_repo``, and ``worker_repo``.
    """
    sock_path = _validate_socket_path(socket_path)
    coerced_timeout = _validate_timeout(timeout_secs)
    rid = _next_request_id()
    request = {"op": "ping", "request_id": rid}
    return _round_trip(sock_path, request, timeout_secs=coerced_timeout)


def client_compile_node(
    socket_path: Any,
    repo: Any,
    node_name: Any,
    *,
    timeout_secs: float,
) -> Mapping[str, Any]:
    """Compile a single Tablet node.

    Mirrors :func:`trellis.atomic_actions.observations.compile_node`.
    Returns the same dict shape today's direct-call observation
    returns: ``returncode``, ``stdout``, ``stderr``, ``timed_out``,
    ``spawn_error``, ``node`` plus the ``requested_nodes`` /
    ``materialized_nodes`` fields that ``materialize_tablet_oleans``
    (which ``compile_node`` wraps) carries.

    The ``repo`` argument is accepted for parity with the observation
    function signature (and so callers can swap in ``client_*`` for
    ``observation_*`` mechanically) but is intentionally NOT sent on
    the wire. The server derives the supervisor repo from the socket's
    runtime root (mitigation 2 of the threat model).
    """
    sock_path = _validate_socket_path(socket_path)
    name = _validate_node_name(node_name)
    coerced_timeout = _validate_timeout(timeout_secs)
    _ = repo  # acknowledged but not sent

    rid = _next_request_id()
    request = {
        "op": "lean_compile_node",
        "request_id": rid,
        "node_name": name,
        "timeout_secs": coerced_timeout,
    }
    return _round_trip(sock_path, request, timeout_secs=coerced_timeout)


def client_materialize_tablet_oleans(
    socket_path: Any,
    repo: Any,
    nodes: Any,
    *,
    timeout_secs: float,
) -> Mapping[str, Any]:
    """Materialize the olean closure for ``nodes``.

    Mirrors :func:`trellis.atomic_actions.observations.materialize_tablet_oleans`.
    Returns the same dict shape: ``requested_nodes``,
    ``materialized_nodes``, ``returncode``, ``stdout``, ``stderr``,
    ``timed_out``, ``spawn_error``.
    """
    sock_path = _validate_socket_path(socket_path)
    validated_nodes = _validate_nodes(nodes)
    coerced_timeout = _validate_timeout(timeout_secs)
    _ = repo

    rid = _next_request_id()
    request = {
        "op": "materialize_oleans",
        "request_id": rid,
        "nodes": list(validated_nodes),
        "timeout_secs": coerced_timeout,
    }
    return _round_trip(sock_path, request, timeout_secs=coerced_timeout)


def client_lean_semantic_payloads(
    socket_path: Any,
    repo: Any,
    nodes: Any,
    *,
    timeout_secs: float,
) -> Mapping[str, Mapping[str, Any]]:
    """Extract semantic-payload fingerprints for each node.

    Mirrors :func:`trellis.atomic_actions.observations.observe_lean_semantic_payloads`.
    Returns the per-node mapping (``{node_name: {"ok": bool,
    "payload": str, "error": str}, ...}``). Note the observation
    function returns the per-node dict directly (no envelope); the
    client unwraps the server's ``{"request_id": ..., "nodes": {...}}``
    envelope so callers see the same shape.
    """
    sock_path = _validate_socket_path(socket_path)
    validated_nodes = _validate_nodes(nodes)
    coerced_timeout = _validate_timeout(timeout_secs)
    _ = repo

    rid = _next_request_id()
    request = {
        "op": "lean_semantic_payloads",
        "request_id": rid,
        "nodes": list(validated_nodes),
        "timeout_secs": coerced_timeout,
    }
    response = _round_trip(sock_path, request, timeout_secs=coerced_timeout)
    nodes_payload = response.get("nodes")
    if not isinstance(nodes_payload, dict):
        raise CheckerRpcError(
            "malformed_response",
            f"lean_semantic_payloads response missing 'nodes' dict, got {type(nodes_payload).__name__}",
        )
    return nodes_payload


def client_print_axioms(
    socket_path: Any,
    repo: Any,
    node_name: Any,
    *,
    timeout_secs: float,
) -> Mapping[str, Any]:
    """Run ``#print axioms <node_name>``.

    Mirrors :func:`trellis.atomic_actions.observations.print_axioms`.
    Returns the completed-process payload plus ``node``.
    """
    sock_path = _validate_socket_path(socket_path)
    name = _validate_node_name(node_name)
    coerced_timeout = _validate_timeout(timeout_secs)
    _ = repo

    rid = _next_request_id()
    request = {
        "op": "print_axioms",
        "request_id": rid,
        "node_name": name,
        "timeout_secs": coerced_timeout,
    }
    return _round_trip(sock_path, request, timeout_secs=coerced_timeout)


def client_local_closure_axioms(
    socket_path: Any,
    node_name: Any,
    *,
    timeout_secs: float = 3600.0,
    no_axcheck: bool = False,
) -> Mapping[str, Any]:
    """Run the Patch A local-closure probe for ``node_name``.

    ``timeout_secs`` defaults to 3600.0 (matches ``LEAN_SUPPORT_TIMEOUT_SECS``
    in :mod:`trellis.atomic_actions.observations`). Production callers route
    through the CLI and pass a value explicitly; this default fires only on
    direct in-process callers and is pinned by
    ``tests/test_checker_client.py::test_client_local_closure_default_timeout``.

    Mirrors the request/response shape of
    :func:`client_print_axioms`, but invokes the Lean-native local-closure
    collector at ``scripts/lean_local_closure.lean`` (see
    ``LOCAL_CLOSURE_IMPL_PLAN.md`` §5.3).

    The response envelope carries the script's verbatim
    ``status``/``kernel_axioms``/``boundary_theorems``/
    ``strict_theorem_deps``/``strict_definition_deps``/``errors`` fields
    alongside transport-level ``returncode``/``timed_out``/``stdout``/
    ``stderr`` for failure-mode introspection. Per plan §5.7 this op is
    server-only — there is no host-lake fallback. Callers that need a
    fallback path should branch on ``TRELLIS_CHECKER_SOCKET`` themselves
    and surface a clear error when unset.

    ``no_axcheck`` (plan §4.6.1 disable flag): when True, the server
    appends ``--no-axcheck`` to the Lean script CLI so the secondary
    axiomization collector is skipped. The response's
    ``axiomization_check`` sub-object then carries ``skipped: true`` and
    the Rust wrapper accepts the (skipped) cross-check trivially.
    Default False (run both collectors).

    Note: unlike the other ``client_*`` helpers in this module, this
    function does NOT take a ``repo`` argument. Patch A's design (plan
    §2.3) keeps the trust boundary tight: the server derives the
    supervisor repo from the socket's runtime root and the worker side
    must not even pretend to have a say in it. There is no observation
    counterpart whose signature we need to mirror here (per plan §5.7,
    Patch A's Python integration ends at the dispatch layer).
    """
    sock_path = _validate_socket_path(socket_path)
    name = _validate_node_name(node_name)
    coerced_timeout = _validate_timeout(timeout_secs)

    rid = _next_request_id()
    request: Dict[str, Any] = {
        "op": "local_closure_axioms",
        "request_id": rid,
        "node_name": name,
        "timeout_secs": coerced_timeout,
    }
    if no_axcheck:
        request["no_axcheck"] = True
    return _round_trip(sock_path, request, timeout_secs=coerced_timeout)


def client_build_tablet(
    socket_path: Any,
    repo: Any,
    *,
    timeout_secs: float,
) -> Mapping[str, Any]:
    """Run ``lake build Tablet`` on the supervisor side.

    Mirrors :func:`trellis.atomic_actions.observations.build_tablet`.
    Returns the completed-process payload (``returncode``, ``stdout``,
    ``stderr``, ``timed_out``, ``spawn_error``).
    """
    sock_path = _validate_socket_path(socket_path)
    coerced_timeout = _validate_timeout(timeout_secs)
    _ = repo

    rid = _next_request_id()
    request = {
        "op": "lean_build_tablet",
        "request_id": rid,
        "timeout_secs": coerced_timeout,
    }
    return _round_trip(sock_path, request, timeout_secs=coerced_timeout)


def client_prepare_compiled_support(
    socket_path: Any,
    repo: Any,
    *,
    timeout_secs: float,
) -> Mapping[str, Any]:
    """Run the ``cache get`` step on the supervisor side.

    Mirrors :func:`trellis.atomic_actions.observations.prepare_compiled_support`.
    Returns the same dict: ``steps_completed``, ``returncode``,
    ``stdout``, ``stderr``, ``timed_out``, ``spawn_error``.
    """
    sock_path = _validate_socket_path(socket_path)
    coerced_timeout = _validate_timeout(timeout_secs)
    _ = repo

    rid = _next_request_id()
    request = {
        "op": "prepare_compiled_support",
        "request_id": rid,
        "timeout_secs": coerced_timeout,
    }
    return _round_trip(sock_path, request, timeout_secs=coerced_timeout)


__all__ = [
    "CheckerRpcError",
    "TRELLIS_CHECKER_SOCKET_ENV",
    "client_build_tablet",
    "client_compile_node",
    "client_lean_semantic_payloads",
    "client_local_closure_axioms",
    "client_materialize_tablet_oleans",
    "client_ping",
    "client_prepare_compiled_support",
    "client_print_axioms",
]
