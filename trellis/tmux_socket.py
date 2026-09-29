"""Dedicated tmux socket for trellis.

All trellis tmux activity (viewer, supervisor run session, per-lane agent
sessions) uses a named tmux server addressed via `tmux -L <socket>`. This
isolates trellis from the user's default tmux daemon: a stray `tmux
kill-server` (without -L) on the host nukes only the user's default socket
and leaves trellis running.

Override the socket name with `TRELLIS_TMUX_SOCKET=<name>`.
"""
from __future__ import annotations

import os

DEFAULT_SOCKET = "trellis"


def tmux_socket() -> str:
    return os.environ.get("TRELLIS_TMUX_SOCKET", DEFAULT_SOCKET) or DEFAULT_SOCKET


def tmux_argv(*args: str) -> list[str]:
    """Build a tmux argv with the trellis socket prefix prepended."""
    return ["tmux", "-L", tmux_socket(), *args]
