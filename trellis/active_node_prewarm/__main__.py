"""CLI entrypoint: run the active-node prewarm server as a long-lived,
supervisor-managed process (sibling to the authoritative checker / Loogle).

Lifecycle mirrors the checker: start the warm server, optionally prewarm an
initial active node, serve the unix socket, and shut down gracefully on
SIGTERM/SIGINT/SIGHUP — reaping the ``lean --server`` child so no orphan
survives supervisor exit.

INERT unless configured (``active_node_prewarm.enabled`` in
``trellis.config.json``). Runs against a SUPERVISOR-CONTROLLED workspace (its
OWN ``.lake``), never the authoritative checker workspace, never a worker
bwrap, never host-lake.

Usage::

    python3 -m trellis.active_node_prewarm <workspace> <socket_path> \
        [--config trellis.config.json] [--active <Node>] [--log-level INFO]

The supervisor pushes active-node changes over the socket (``set_active``); the
``--active`` flag only seeds the FIRST node at startup.
"""

from __future__ import annotations

import argparse
import logging
import signal
import sys
from pathlib import Path
from typing import Any, List, Optional

from trellis.active_node_prewarm.config import ActiveNodePrewarmConfig
from trellis.active_node_prewarm.server import ActiveNodePrewarmServer

_LOGGER = logging.getLogger("trellis.active_node_prewarm.main")


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="trellis.active_node_prewarm")
    parser.add_argument("workspace", type=Path, help="supervisor-controlled workspace")
    parser.add_argument("socket_path", type=Path, help="unix socket to bind")
    parser.add_argument("--config", type=Path, default=None)
    parser.add_argument("--active", default="", help="initial active node to prewarm")
    parser.add_argument("--log-level", default="INFO")
    args = parser.parse_args(argv)

    logging.basicConfig(
        level=args.log_level.upper(),
        format="%(asctime)s %(levelname)s %(name)s :: %(message)s",
    )

    if args.config is not None:
        cfg = ActiveNodePrewarmConfig.load(args.config)
    else:
        cfg = ActiveNodePrewarmConfig(enabled=True)

    if not cfg.enabled:
        _LOGGER.info("active_node_prewarm disabled in config; exiting inert.")
        return 0

    server = ActiveNodePrewarmServer(
        workspace=args.workspace, config=cfg, socket_path=args.socket_path
    )
    server.start()
    if args.active.strip():
        status = server.set_active_node(args.active.strip())
        _LOGGER.info("initial prewarm of %s: %s", args.active.strip(), status)

    def _handle_signal(_signum: int, _frame: Any) -> None:
        server.shutdown()

    signal.signal(signal.SIGTERM, _handle_signal)
    signal.signal(signal.SIGINT, _handle_signal)
    signal.signal(signal.SIGHUP, _handle_signal)

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.shutdown()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
