"""Worker-facing incremental Isabelle checker (``incremental-check``).

The Isabelle analogue of :mod:`trellis.incremental_check` (the Lean warm
``lean --server`` advisory). An advisory inner-loop accelerator that drives the
supervisor's WARM Isabelle checker session over the checker socket to give an
``isabelle build``-shaped pass/fail+errors in a fraction of the time — the warm
session re-elaborates ONLY the in-flight theory against the warm accepted
prefix (the accepted sibling nodes + ``Tablet_Preamble``), so a re-check is
sub-second after the first warm-up.

It is NEVER authoritative: green here is necessary but not sufficient. The
deterministic worker check (the ``isabelle-thm-deps`` cert / the node-check
gate, via the checker socket) remains the only sign-off — this drives the
lightweight ``isabelle_warm_advisory`` op, which runs the warm ``check_node``
WITHOUT the cert or the cold cross-check (that is the gate's job).

Why a thin socket forwarder (not an in-sandbox warm process, unlike the Lean
broker): the warm Isabelle session lives in the supervisor's checker server
(its base heap + the accepted-node prefix are warm there for the whole run); the
worker reaches it over ``TRELLIS_CHECKER_SOCKET`` (already RO-bound into the
sandbox). A worker-side Isabelle process would have to reload the multi-second
base heap every burst and could not hold the accepted prefix warm. So the fast
loop IS a socket round-trip to the warm checker, and this module is the thin
client + the ``isabelle build``-shaped renderer.

Failure-open (everywhere): on ANY failure — socket unset/unreachable, the warm
flag off, the node not synced yet, a warm-session anomaly, a protocol error, a
timeout — the advisory reports ``advisory_unavailable`` and this client prints a
notice and exits 0 (a green-or-unknown advisory never blocks). The worker then
confirms / falls back with ``isabelle build`` (the authoritative-shape tool),
exactly as the Lean advisory falls back to ``lake build``. Worst case is wasted
worker time, never a wrong "pass".

This module is SELF-CONTAINED (no ``trellis`` package import) so the provisioned
copy runs standalone inside the worker sandbox without the package on
``sys.path`` — mirroring :mod:`trellis.incremental_check`. The socket framing
mirrors :mod:`trellis.checker.protocol` (newline-delimited JSON) and
:mod:`trellis.atomic_actions.checker_client` (the ``auth_token`` injection);
both are duplicated as bare literals here for the same standalone reason.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import socket
import sys
from typing import Any, Dict, Mapping, Optional, Sequence


# Single source of truth for the node-name shape — mirrors
# trellis.checker.protocol.NODE_NAME_REGEX_STR. Duplicated as a bare literal
# (no package import) so the provisioned script runs standalone inside the
# worker sandbox without the trellis package on sys.path.
_NODE_NAME_REGEX = re.compile(r"\A[A-Za-z][A-Za-z0-9_]*\Z")
_NODE_NAME_MAX_LEN = 128

# Wire caps + env-var names — mirror trellis.checker.protocol /
# trellis.atomic_actions.checker_client (duplicated, standalone).
_MAX_LINE_BYTES = 64 * 1024
_MAX_MESSAGE_BYTES = 256 * 1024 * 1024
_CHECKER_SOCKET_ENV = "TRELLIS_CHECKER_SOCKET"
_CHECKER_TOKEN_ENV = "TRELLIS_CHECKER_TOKEN"

# Advisory round-trip budget. The warm check is sub-second once warm, but the
# very first call in a run pays the warm-session boot (~10s) + the
# accepted-prefix promote, so give generous slack; the supervisor caps the real
# work server-side. Overridable for tests.
_ADVISORY_TIMEOUT_SECS = float(
    os.environ.get("ISABELLE_INCREMENTAL_CHECK_TIMEOUT", "1800")
)


# --------------------------------------------------------------------------
# Sandbox guard (mirrors incremental_check.py MINOR-5)
# --------------------------------------------------------------------------

def _sandbox_markers() -> bool:
    """True iff we appear to be inside the worker bwrap (TMPDIR tmpfs marker).

    Mirrors :func:`trellis.incremental_check._sandbox_markers`. Unlike the Lean
    advisory (which spawns a process and must never host-lake), this client only
    opens a unix socket, so the guard is advisory: it gates the
    ``--prewarm``/normal paths to the sandbox but the consequence of running it
    outside is merely a confused connection, not a host-mutating action. We keep
    the marker check for symmetry + an escape hatch for the offline harness.
    """
    tmpdir = os.environ.get("TMPDIR", "")
    return tmpdir == "/trellis-tmp" and os.path.isdir("/trellis-tmp")


def _allow_unsandboxed() -> bool:
    """Escape hatch for the offline validation harness ONLY.

    ``ISABELLE_INCREMENTAL_CHECK_ALLOW_UNSANDBOXED=1`` bypasses the sandbox
    marker check (mirrors the Lean module's separate-env-var convention so it
    cannot be tripped by a live-run variable).
    """
    return os.environ.get("ISABELLE_INCREMENTAL_CHECK_ALLOW_UNSANDBOXED", "") == "1"


# --------------------------------------------------------------------------
# Node-name parsing — accept Tablet.X, Tablet_X, or bare X; return X
# --------------------------------------------------------------------------

def _validate_node_name(node_name: str) -> str:
    cleaned = node_name.strip()
    if not cleaned:
        raise ValueError("node name is empty")
    if len(cleaned) > _NODE_NAME_MAX_LEN:
        raise ValueError(f"node name exceeds {_NODE_NAME_MAX_LEN} chars")
    if _NODE_NAME_REGEX.fullmatch(cleaned) is None:
        raise ValueError(f"node name does not match identifier shape: {cleaned!r}")
    return cleaned


def _parse_target(arg: str) -> str:
    """Accept ``Tablet.NodeName`` / ``Tablet_NodeName`` / bare ``NodeName``.

    The worker UX mirrors the Lean ``incremental-check Tablet.NodeName``; the
    Isabelle theory name is ``Tablet_<Node>`` so a ``Tablet_`` prefix is also
    accepted. Either prefix is stripped to the bare node id the socket op takes.
    """
    raw = arg.strip()
    if raw.startswith("Tablet."):
        raw = raw[len("Tablet."):]
    elif raw.startswith("Tablet_"):
        raw = raw[len("Tablet_"):]
    return _validate_node_name(raw)


# --------------------------------------------------------------------------
# Newline-delimited JSON socket round-trip (mirrors checker_client)
# --------------------------------------------------------------------------

def _socket_path() -> Optional[str]:
    raw = os.environ.get(_CHECKER_SOCKET_ENV, "")
    stripped = raw.strip()
    return stripped or None


def _maybe_auth_token() -> Optional[str]:
    raw = os.environ.get(_CHECKER_TOKEN_ENV, "")
    token = raw.strip()
    return token or None


def _round_trip(
    sock_path: str, request: Mapping[str, Any], *, timeout_secs: float
) -> Mapping[str, Any]:
    """Open a fresh AF_UNIX connection, send one request, read one response.

    Raises :class:`_AdvisoryUnavailable` (with a human reason) on ANY transport
    failure so the caller can render the failure-open notice + exit 0. The
    server's ``rpc_error`` envelope is also mapped to ``_AdvisoryUnavailable``
    (a malformed/unknown-op server is treated as "advisory unavailable", never
    a green).
    """
    payload: Dict[str, Any] = dict(request)
    token = _maybe_auth_token()
    if token is not None:
        payload["auth_token"] = token

    encoded = json.dumps(payload, separators=(",", ":"), ensure_ascii=False).encode(
        "utf-8"
    ) + b"\n"
    if len(encoded) > _MAX_LINE_BYTES:
        raise _AdvisoryUnavailable(
            f"advisory request of {len(encoded)} bytes exceeds {_MAX_LINE_BYTES}"
        )

    connect_budget = min(timeout_secs, 10.0)
    read_deadline = max(timeout_secs + 5.0, connect_budget)

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.settimeout(connect_budget)
        try:
            sock.connect(sock_path)
        except (FileNotFoundError, ConnectionRefusedError, PermissionError) as exc:
            raise _AdvisoryUnavailable(f"checker socket unreachable: {exc}")
        except socket.timeout:
            raise _AdvisoryUnavailable(
                f"timed out connecting to checker after {connect_budget}s"
            )
        except OSError as exc:
            raise _AdvisoryUnavailable(f"could not reach checker: {exc}")

        sock.settimeout(read_deadline)
        try:
            sock.sendall(encoded)
        except OSError as exc:
            raise _AdvisoryUnavailable(f"sending advisory request failed: {exc}")

        buffer = bytearray()
        while True:
            idx = buffer.find(b"\n")
            if idx >= 0:
                line = bytes(buffer[:idx])
                break
            if len(buffer) > _MAX_MESSAGE_BYTES:
                raise _AdvisoryUnavailable(
                    f"advisory response exceeded {_MAX_MESSAGE_BYTES} bytes"
                )
            try:
                chunk = sock.recv(8192)
            except socket.timeout:
                raise _AdvisoryUnavailable(
                    f"timed out waiting for advisory response after {read_deadline}s"
                )
            except OSError as exc:
                raise _AdvisoryUnavailable(f"reading advisory response failed: {exc}")
            if not chunk:
                raise _AdvisoryUnavailable(
                    "checker closed the connection before a response"
                )
            buffer.extend(chunk)
    finally:
        try:
            sock.close()
        except OSError:
            pass

    try:
        decoded = json.loads(line.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise _AdvisoryUnavailable(f"advisory response was not valid JSON: {exc}")
    if not isinstance(decoded, dict):
        raise _AdvisoryUnavailable("advisory response was not a JSON object")
    if "rpc_error" in decoded:
        rpc = decoded.get("rpc_error")
        if isinstance(rpc, dict):
            kind = rpc.get("kind", "rpc_error")
            message = rpc.get("message", "")
        else:
            kind, message = "rpc_error", str(rpc)
        raise _AdvisoryUnavailable(f"checker rpc_error ({kind}): {message}")
    return decoded


class _AdvisoryUnavailable(Exception):
    """The warm advisory could not be obtained (transport/flag/sync/anomaly).

    Carries a human reason; the caller renders the failure-open notice and
    exits 0 (advising an ``isabelle build`` confirm/fallback). NEVER a green.
    """


# --------------------------------------------------------------------------
# Advisory call + render
# --------------------------------------------------------------------------

def _request_advisory(node_name: str, *, timeout_secs: float) -> Mapping[str, Any]:
    sock_path = _socket_path()
    if sock_path is None:
        raise _AdvisoryUnavailable(
            f"{_CHECKER_SOCKET_ENV} is not set (the warm checker socket is the "
            "only advisory transport)"
        )
    request = {
        "op": "isabelle_warm_advisory",
        "request_id": _next_request_id(),
        "node_name": node_name,
        "timeout_secs": float(timeout_secs),
    }
    return _round_trip(sock_path, request, timeout_secs=timeout_secs)


_request_id_seed = os.getpid() << 16


def _next_request_id() -> int:
    global _request_id_seed
    _request_id_seed += 1
    # Keep it within a safe positive int range the server accepts.
    return _request_id_seed & 0x7FFFFFFF


def _render_unavailable(node_name: str, reason: str) -> int:
    """Failure-open: print the notice, exit 0 (advisory never blocks)."""
    sys.stderr.write(
        f"incremental-check: warm advisory unavailable for Tablet_{node_name} "
        f"({reason}); confirm with `isabelle build` (the authoritative-shape "
        f"tool). This advisory is non-blocking — the deterministic worker check "
        f"is the only sign-off.\n"
    )
    return 0


def _render_verdict(node_name: str, response: Mapping[str, Any]) -> int:
    """Render the advisory verdict ``isabelle build``-shaped; return exit code.

    ``advisory_unavailable`` → failure-open notice + exit 0. Otherwise print the
    pass/fail; a fail surfaces the ``*** …`` error lines and exits nonzero.
    """
    if response.get("advisory_unavailable"):
        errs = response.get("errors") or []
        reason = "; ".join(str(e) for e in errs) if errs else "warm advisory unavailable"
        return _render_unavailable(node_name, reason)

    ok = bool(response.get("ok"))
    if ok:
        seconds = response.get("seconds")
        secs = f" in {float(seconds):.2f}s" if isinstance(seconds, (int, float)) else ""
        print(
            f"incremental-check: Tablet_{node_name} OK{secs} (advisory warm "
            f"pre-check; the deterministic worker check is the only sign-off)."
        )
        return 0

    errors = response.get("errors") or []
    for line in errors:
        print(str(line))
    if not errors:
        print(f"Tablet_{node_name}: error: warm advisory reported the proof did not check")
    print(
        f"incremental-check: Tablet_{node_name} FAILED the advisory warm pre-check "
        f"({len(errors)} error line(s)); fix and re-check. Confirm a final green "
        f"with `isabelle build`."
    )
    return 1


def _run_incremental(node_name: str, *, timeout_secs: float) -> int:
    try:
        response = _request_advisory(node_name, timeout_secs=timeout_secs)
    except _AdvisoryUnavailable as exc:
        return _render_unavailable(node_name, str(exc))
    except Exception as exc:  # noqa: BLE001 — failure-open on anything unexpected
        return _render_unavailable(node_name, f"{type(exc).__name__}: {exc}")
    return _render_verdict(node_name, response)


def _run_prewarm(node_name: str, *, timeout_secs: float) -> int:
    """Pay the warm-session boot + accepted-prefix promote at burst BEGIN.

    Drives one advisory so the warm session is up and the accepted prefix is
    promoted before the worker's first edit. Fully failure-open: any problem is
    logged and we exit 0 (a prewarm must NEVER break the burst, and it prints no
    pass/fail verdict — it is a pure side-effecting warmup, mirroring the Lean
    ``--prewarm``).
    """
    try:
        response = _request_advisory(node_name, timeout_secs=timeout_secs)
        state = (
            "unavailable"
            if response.get("advisory_unavailable")
            else ("ok" if response.get("ok") else "proof-incomplete")
        )
        sys.stderr.write(
            f"incremental-check: prewarmed Tablet_{node_name} (warm advisory: "
            f"{state}); the warm session is now up.\n"
        )
    except Exception as exc:  # noqa: BLE001 — never let a prewarm break the burst
        sys.stderr.write(
            f"incremental-check: prewarm of Tablet_{node_name} failed "
            f"({type(exc).__name__}: {exc}); ignoring (the first real check will "
            f"warm the session).\n"
        )
    return 0


# --------------------------------------------------------------------------
# CLI (mirrors incremental_check.py so the worker UX is symmetric with Lean)
# --------------------------------------------------------------------------

def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="incremental-check",
        description=(
            "Advisory warm Isabelle pre-check. Green is necessary but not "
            "sufficient; the deterministic worker check is the only sign-off."
        ),
    )
    parser.add_argument(
        "target",
        nargs="?",
        help="Tablet.NodeName / Tablet_NodeName / bare NodeName to check.",
    )
    parser.add_argument(
        "--repo",
        default=".",
        help=(
            "Repo root (accepted for parity with the Lean incremental-check; the "
            "warm checker derives the repo from the socket runtime root, so it is "
            "NOT sent on the wire)."
        ),
    )
    parser.add_argument(
        "--shutdown",
        action="store_true",
        help=(
            "No-op for the Isabelle advisory (the warm session lives in the "
            "supervisor checker, not a per-burst process); accepted for parity."
        ),
    )
    parser.add_argument(
        "--prewarm",
        action="store_true",
        help=(
            "Drive one advisory so the warm session + accepted prefix are warm "
            "before the first edit. Failure-open: always exits 0, prints no "
            "verdict."
        ),
    )
    parser.add_argument(
        "--start",
        dest="prewarm",
        action="store_true",
        help=argparse.SUPPRESS,  # alias for --prewarm.
    )
    args = parser.parse_args(list(argv) if argv is not None else None)

    _ = args.repo  # accepted, not sent (the server derives the repo)

    if args.shutdown:
        # The warm Isabelle session is owned by the supervisor checker and is not
        # torn down per burst — nothing to shut down here. Exit cleanly.
        return 0

    if not _sandbox_markers() and not _allow_unsandboxed():
        sys.stderr.write(
            "incremental-check must run inside the worker sandbox "
            f"(expected TMPDIR=/trellis-tmp, got {os.environ.get('TMPDIR', '')!r}).\n"
        )
        return 2

    if not args.target:
        parser.error("target (Tablet.NodeName) is required")

    try:
        node_name = _parse_target(args.target)
    except ValueError as exc:
        sys.stderr.write(f"incremental-check: {exc}\n")
        # A prewarm must never break the burst, even on a bad node name.
        return 0 if args.prewarm else 2

    if args.prewarm:
        return _run_prewarm(node_name, timeout_secs=_ADVISORY_TIMEOUT_SECS)

    return _run_incremental(node_name, timeout_secs=_ADVISORY_TIMEOUT_SECS)


if __name__ == "__main__":
    raise SystemExit(main())
