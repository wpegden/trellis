"""Supervisor-side wiring for the active-node prewarm server.

Two roles:

1. ``maybe_prewarm_active_node`` — the bridge hook. Called from
   ``trellis.runtime.bridge._handle_worker`` just before the worker burst is
   launched. GATED on ``active_node_prewarm.enabled`` (off by default) AND on
   the server's socket being exported (``TRELLIS_ACTIVE_NODE_PREWARM_SOCKET``).
   When both hold it extracts the kernel-held active node from the worker
   request and sends a ``set_active`` message to the long-lived prewarm server,
   so the server has paid the cold elaboration before the worker's first check.
   Best-effort: any failure is swallowed (prewarm warmth is never correctness),
   so it can NEVER break burst dispatch.

   The bridge is a one-shot per-burst subprocess, so it cannot itself host the
   persistent server. The persistent server is launcher-managed (a sibling
   process, mirroring the authoritative checker); this hook only nudges it over
   its socket and exports the socket to the worker burst.

2. ``export_socket_to_worker_env`` — copy the server socket path into the env
   the worker burst inherits, under the name the worker-side client reads
   (``INCREMENTAL_CHECK_ACTIVE_PREWARM_SOCK``). Only when enabled + socket
   present; otherwise a no-op, keeping the worker-side preference inert.

Both functions are pure no-ops unless the feature is enabled AND the server
socket env var is set, so wiring them in cannot perturb a live run that has not
opted in.
"""

from __future__ import annotations

import json
import logging
import os
import socket
import struct
from pathlib import Path
from typing import Any, Dict, Mapping, MutableMapping, Optional

from trellis.active_node_prewarm.config import ActiveNodePrewarmConfig

_LOGGER = logging.getLogger("trellis.active_node_prewarm.hook")

# Env var the launcher-managed server exports so the bridge can reach it.
SERVER_SOCKET_ENV = "TRELLIS_ACTIVE_NODE_PREWARM_SOCKET"
# Env var the worker-side client (trellis.incremental_check) reads to find the
# server. Must match _ACTIVE_PREWARM_SOCK_ENV there.
WORKER_SOCKET_ENV = "INCREMENTAL_CHECK_ACTIVE_PREWARM_SOCK"


def _server_socket_path() -> Optional[Path]:
    raw = os.environ.get(SERVER_SOCKET_ENV, "").strip()
    if not raw:
        return None
    p = Path(raw)
    return p if p.exists() else None


def _extract_active_node(request: Mapping[str, Any]) -> Optional[str]:
    """Best-effort extraction of the kernel-held active node from a worker
    request. Returns the node name, or None if it cannot be determined safely
    (the hook then skips prewarm — failure-open).

    The kernel surfaces ``active_node`` in a few places depending on the
    validation kind; we check them in order of specificity. We never GUESS
    (e.g. from authorized_nodes, which may be a wider scope) — only an explicit
    ``active_node`` counts.
    """
    # 1. Top-level (if the kernel adds an explicit field).
    val = request.get("active_node")
    if isinstance(val, str) and val.strip():
        return val.strip()

    # 2. worker_context.active_node.
    rs = request.get("request_summary")
    if isinstance(rs, Mapping):
        wc = rs.get("worker_context")
        if isinstance(wc, Mapping):
            v = wc.get("active_node")
            if isinstance(v, str) and v.strip():
                return v.strip()

    # 3. The validation execution plan steps (proof_easy_scope /
    #    proof_worker_delta carry active_node).
    plan = request.get("validation_execution_plan")
    if isinstance(plan, Mapping):
        plan = plan.get("steps")
    if isinstance(plan, list):
        for step in plan:
            if isinstance(step, Mapping):
                v = step.get("active_node")
                if isinstance(v, str) and v.strip():
                    return v.strip()

    # 4. A contract object carrying active_node.
    contract = request.get("contract")
    if isinstance(contract, Mapping):
        v = contract.get("active_node")
        if isinstance(v, str) and v.strip():
            return v.strip()

    return None


def _send_set_active(sock_path: Path, node: str, timeout_secs: float = 30.0) -> Optional[Dict[str, Any]]:
    """Send a ``set_active`` message to the prewarm server. Returns the status
    dict on success, or None on any failure (best-effort)."""
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(timeout_secs)
            s.connect(str(sock_path))
            body = json.dumps({"method": "set_active", "node": node}).encode("utf-8")
            s.sendall(struct.pack(">I", len(body)) + body)
            head = s.recv(4)
            if len(head) < 4:
                return None
            (length,) = struct.unpack(">I", head)
            if length == 0 or length > (64 * 1024 * 1024):
                return None
            buf = b""
            while len(buf) < length:
                chunk = s.recv(length - len(buf))
                if not chunk:
                    return None
                buf += chunk
            return json.loads(buf.decode("utf-8"))
    except (OSError, ValueError):
        return None


def maybe_prewarm_active_node(
    *,
    config_path: Optional[Path],
    request: Mapping[str, Any],
    timeout_secs: float = 30.0,
) -> Optional[Dict[str, Any]]:
    """Prewarm the active node on the supervisor server, if enabled + reachable.

    Returns the server's status dict, or None when the feature is disabled /
    not configured / unreachable / the active node cannot be determined — in all
    of which cases prewarm is silently skipped. Never raises.
    """
    try:
        cfg = (
            ActiveNodePrewarmConfig.load(config_path)
            if config_path is not None
            else ActiveNodePrewarmConfig()
        )
        if not cfg.enabled:
            return None
        sock_path = _server_socket_path()
        if sock_path is None:
            return None
        node = _extract_active_node(request)
        if node is None:
            _LOGGER.debug("active-node prewarm: could not determine active node; skipping")
            return None
        status = _send_set_active(sock_path, node, timeout_secs=timeout_secs)
        if status is not None:
            _LOGGER.info("active-node prewarm of %s: %s", node, status)
        return status
    except Exception as exc:  # never break burst dispatch
        _LOGGER.debug("active-node prewarm hook error (ignored): %s", exc)
        return None


def export_socket_to_worker_env(
    env: MutableMapping[str, str], *, config_path: Optional[Path]
) -> None:
    """If the feature is enabled and the server socket is exported, copy the
    socket path into ``env`` under the worker-client name so the worker's
    ``incremental-check`` can prefer the warm active-node server. No-op
    otherwise (keeps the worker-side preference inert)."""
    try:
        cfg = (
            ActiveNodePrewarmConfig.load(config_path)
            if config_path is not None
            else ActiveNodePrewarmConfig()
        )
        if not cfg.enabled:
            return
        sock_path = _server_socket_path()
        if sock_path is None:
            return
        env[WORKER_SOCKET_ENV] = str(sock_path)
    except Exception:
        return


# Env names the worker burst reads. Must match codex_headless's prewarm hook
# and the standalone incremental_check giant-node env overrides.
WORKER_PREWARM_FLAG_ENV = "TRELLIS_INCREMENTAL_PREWARM"
WORKER_PREWARM_NODE_ENV = "TRELLIS_PREWARM_NODE"
WORKER_GIANT_MAX_LINES_ENV = "INCREMENTAL_CHECK_GIANT_MAX_LINES"
WORKER_GIANT_HEARTBEAT_ENV = "INCREMENTAL_CHECK_GIANT_HEARTBEAT_CEILING"


def export_worker_burst_env(
    env: MutableMapping[str, str],
    *,
    config_path: Optional[Path],
    request: Mapping[str, Any],
) -> None:
    """Populate the worker-burst env for the prewarm feature, gated on
    ``active_node_prewarm.enabled``.

    Sets (only when enabled):
      * the active-node prewarm SERVER socket (if exported) under the
        worker-client name — preference step 1;
      * the in-burst ``--prewarm`` hook flag + active node, so the worker's
        burst script kicks a backgrounded ``incremental-check --prewarm`` —
        preference step 2's cold start paid at burst begin;
      * the giant-node skip thresholds, so the standalone worker copy classifies
        giants identically to the supervisor server.

    These ride into the bwrap via ``sandbox._PASSTHROUGH_VALUE_ENV_VARS``
    (the function writes into ``os.environ``, which the passthrough reads at
    wrap time). No-op unless enabled, so it cannot perturb a non-opted-in run.
    Never raises (a prewarm wiring error must not break burst dispatch)."""
    try:
        cfg = (
            ActiveNodePrewarmConfig.load(config_path)
            if config_path is not None
            else ActiveNodePrewarmConfig()
        )
        if not cfg.enabled:
            return
        # Step-1 server socket (if the launcher started the persistent server).
        export_socket_to_worker_env(env, config_path=config_path)
        # Giant-node thresholds for the standalone worker copy.
        env[WORKER_GIANT_MAX_LINES_ENV] = str(cfg.giant_node_max_lines)
        env[WORKER_GIANT_HEARTBEAT_ENV] = str(cfg.giant_node_heartbeat_ceiling)
        # In-burst prewarm of the active node (the cold start paid at burst
        # begin even when the supervisor server is not running).
        node = _extract_active_node(request)
        if node is not None:
            env[WORKER_PREWARM_FLAG_ENV] = "1"
            env[WORKER_PREWARM_NODE_ENV] = node
    except Exception as exc:  # never break burst dispatch
        _LOGGER.debug("active-node prewarm worker-env export error (ignored): %s", exc)
        return
