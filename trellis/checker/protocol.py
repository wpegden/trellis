"""Wire protocol for the unified checker server.

Line-delimited JSON over ``SOCK_STREAM``. Each request is one JSON object
terminated by ``\\n``; each response likewise. Request always carries
``op`` (string) and ``request_id`` (int, client-chosen, echoed). Per the
threat model (mitigation 2), ``repo_path`` is **not** part of the request
schema — the server derives it from the socket's filesystem location.

This module also owns the single source of truth for the node-name regex
that gates every path-construction site in the dispatcher (Tablet/{X}.lean
filename, ``import Tablet.{X}`` source line, ``Tablet.{X}`` build target).
The regex matches the convention documented in ``FILESPEC.md`` for
ordinary node files: identifier-shaped, must start with a letter.
"""

from __future__ import annotations

import json
import math
import re
from dataclasses import dataclass, field
from typing import Any, Mapping, Optional, Sequence


# Single source of truth — anchored, must start with a letter, ASCII identifier
# characters only. See FILESPEC.md "Tablet/<Node>.lean" naming convention.
NODE_NAME_REGEX_STR = r"[A-Za-z][A-Za-z0-9_]*"
NODE_NAME_REGEX = re.compile(rf"\A{NODE_NAME_REGEX_STR}\Z")
NODE_NAME_MAX_LEN = 128
MAX_NODES_PER_REQUEST = 1024
TIMEOUT_SECS_MIN = 1.0
TIMEOUT_SECS_MAX = 3600.0
TIMEOUT_SECS_DEFAULT = TIMEOUT_SECS_MAX

# Wire-level caps; server enforces these too (defence in depth).
MAX_LINE_BYTES = 64 * 1024  # 64 KiB
MAX_MESSAGE_BYTES = 256 * 1024 * 1024  # 256 MiB — sized for full-tablet lean_semantic_payloads responses (~1 MB/node, scales to ~250-node tablets)

KNOWN_OPS = (
    "ping",
    "verify_node",
    "lean_compile_node",
    "materialize_oleans",
    "lean_semantic_payloads",
    "print_axioms",
    "local_closure_axioms",
    "lean_build_tablet",
    "prepare_compiled_support",
)


class ProtocolError(Exception):
    """Raised by validate_request for any malformed envelope.

    Carries an ``rpc_error`` kind so the dispatcher can echo it back in a
    structured envelope without leaking internal details. The kinds are a
    closed set: ``malformed_request``, ``unknown_op``.
    """

    def __init__(self, kind: str, message: str) -> None:
        super().__init__(message)
        self.kind = kind
        self.message = message


@dataclass
class CheckerRequest:
    """Validated request envelope. ``repo_path`` is intentionally absent —
    the server resolves the supervisor repo from the socket's parent
    runtime root."""

    op: str
    request_id: int
    node_name: Optional[str] = None
    nodes: Sequence[str] = field(default_factory=tuple)
    timeout_secs: float = TIMEOUT_SECS_DEFAULT
    raw: Mapping[str, Any] = field(default_factory=dict)


def _coerce_timeout(value: Any) -> float:
    if value is None:
        return TIMEOUT_SECS_DEFAULT
    try:
        coerced = float(value)
    except (TypeError, ValueError):
        raise ProtocolError(
            "malformed_request", f"timeout_secs is not a number: {value!r}"
        )
    if not math.isfinite(coerced):
        raise ProtocolError(
            "malformed_request", f"timeout_secs must be finite, got {value!r}"
        )
    if coerced < TIMEOUT_SECS_MIN:
        return TIMEOUT_SECS_MIN
    if coerced > TIMEOUT_SECS_MAX:
        return TIMEOUT_SECS_MAX
    return coerced


def _validate_node_name(value: Any, *, field_name: str = "node_name") -> str:
    if not isinstance(value, str):
        raise ProtocolError(
            "malformed_request", f"{field_name} must be a string, got {type(value).__name__}"
        )
    if len(value) == 0:
        raise ProtocolError("malformed_request", f"{field_name} is empty")
    if len(value) > NODE_NAME_MAX_LEN:
        raise ProtocolError(
            "malformed_request",
            f"{field_name} exceeds {NODE_NAME_MAX_LEN} chars (got {len(value)})",
        )
    if NODE_NAME_REGEX.fullmatch(value) is None:
        raise ProtocolError(
            "malformed_request",
            f"{field_name} does not match {NODE_NAME_REGEX_STR}: {value!r}",
        )
    return value


def _validate_nodes(value: Any) -> tuple[str, ...]:
    if not isinstance(value, (list, tuple)):
        raise ProtocolError(
            "malformed_request", f"nodes must be a list, got {type(value).__name__}"
        )
    if len(value) > MAX_NODES_PER_REQUEST:
        raise ProtocolError(
            "malformed_request",
            f"nodes list of {len(value)} exceeds cap {MAX_NODES_PER_REQUEST}",
        )
    out: list[str] = []
    for idx, raw in enumerate(value):
        out.append(_validate_node_name(raw, field_name=f"nodes[{idx}]"))
    return tuple(out)


def parse_line(line: bytes) -> Mapping[str, Any]:
    """Decode one newline-terminated wire frame into a Mapping.

    Trailing newline is stripped; lines without one are also accepted
    (the framing layer in the server handles ``EOF`` separately).
    Raises ``ProtocolError(kind="malformed_request")`` on any decode
    failure, including non-UTF8 bytes and non-object JSON.
    """
    if len(line) > MAX_LINE_BYTES:
        raise ProtocolError(
            "malformed_request",
            f"request line exceeds {MAX_LINE_BYTES} bytes (got {len(line)})",
        )
    if line.endswith(b"\n"):
        line = line[:-1]
    try:
        text = line.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise ProtocolError(
            "malformed_request", f"request is not valid UTF-8: {exc}"
        )
    try:
        decoded = json.loads(text)
    except json.JSONDecodeError as exc:
        raise ProtocolError(
            "malformed_request", f"request is not valid JSON: {exc.msg}"
        )
    if not isinstance(decoded, dict):
        raise ProtocolError(
            "malformed_request",
            f"request must be a JSON object, got {type(decoded).__name__}",
        )
    return decoded


def validate_request(payload: Mapping[str, Any]) -> CheckerRequest:
    """Validate one decoded request envelope.

    On success returns a ``CheckerRequest`` with all string/path fields
    sanitised. Raises ``ProtocolError`` otherwise.
    """
    op_raw = payload.get("op")
    if not isinstance(op_raw, str):
        raise ProtocolError(
            "malformed_request", f"op must be a string, got {type(op_raw).__name__}"
        )
    op = op_raw.strip()
    if not op:
        raise ProtocolError("malformed_request", "op is empty")
    if op not in KNOWN_OPS:
        raise ProtocolError("unknown_op", f"unknown op {op!r}")

    rid_raw = payload.get("request_id")
    if isinstance(rid_raw, bool) or not isinstance(rid_raw, int):
        raise ProtocolError(
            "malformed_request",
            f"request_id must be a non-bool integer, got {type(rid_raw).__name__}",
        )
    if rid_raw < 0:
        raise ProtocolError("malformed_request", f"request_id must be >= 0, got {rid_raw}")

    timeout = _coerce_timeout(payload.get("timeout_secs"))

    node_name: Optional[str] = None
    nodes: tuple[str, ...] = ()
    if op in {"verify_node", "lean_compile_node", "print_axioms", "local_closure_axioms"}:
        if "node_name" not in payload:
            raise ProtocolError("malformed_request", f"op {op} requires node_name")
        node_name = _validate_node_name(payload["node_name"])
    elif op in {"materialize_oleans", "lean_semantic_payloads"}:
        if "nodes" not in payload:
            raise ProtocolError("malformed_request", f"op {op} requires nodes")
        nodes = _validate_nodes(payload["nodes"])
    elif op in {"lean_build_tablet", "prepare_compiled_support", "ping"}:
        # No node arguments — these are workspace-scoped or pure control ops.
        pass

    # repo_path explicitly disallowed (mitigation 2: derive server-side).
    if "repo_path" in payload:
        raise ProtocolError(
            "malformed_request",
            "repo_path is not accepted; server derives it from socket location",
        )

    return CheckerRequest(
        op=op,
        request_id=rid_raw,
        node_name=node_name,
        nodes=nodes,
        timeout_secs=timeout,
        raw=dict(payload),
    )


def encode_response(payload: Mapping[str, Any]) -> bytes:
    """Encode a response into one newline-terminated UTF-8 wire frame.

    Raises ``ValueError`` if the encoded payload exceeds
    ``MAX_MESSAGE_BYTES`` so server-side oversized responses fail loudly
    rather than truncate silently.
    """
    text = json.dumps(payload, separators=(",", ":"), ensure_ascii=False)
    encoded = text.encode("utf-8") + b"\n"
    if len(encoded) > MAX_MESSAGE_BYTES:
        raise ValueError(
            f"response payload exceeds {MAX_MESSAGE_BYTES} bytes (got {len(encoded)})"
        )
    return encoded


def rpc_error_envelope(
    request_id: int, kind: str, message: str
) -> dict[str, Any]:
    """Build the standard error envelope for protocol-level failures.

    Lake/lean errors are surfaced inside ``returncode``/``stderr`` of the
    op-specific success envelope; this is reserved for the protocol layer
    (malformed JSON, unknown op, sync failures, internal exceptions, etc.)
    so callers can distinguish a transport failure from a tool failure.
    """
    return {
        "request_id": request_id,
        "rpc_error": {"kind": kind, "message": message},
    }


__all__ = [
    "NODE_NAME_REGEX",
    "NODE_NAME_REGEX_STR",
    "NODE_NAME_MAX_LEN",
    "MAX_NODES_PER_REQUEST",
    "TIMEOUT_SECS_MIN",
    "TIMEOUT_SECS_MAX",
    "TIMEOUT_SECS_DEFAULT",
    "MAX_LINE_BYTES",
    "MAX_MESSAGE_BYTES",
    "KNOWN_OPS",
    "ProtocolError",
    "CheckerRequest",
    "parse_line",
    "validate_request",
    "encode_response",
    "rpc_error_envelope",
]
