"""Off-by-default config for the supervisor-side active-node prewarm server.

Read from ``trellis.config.json`` under an ``active_node_prewarm`` object. The
whole feature is INERT unless ``enabled`` is true (mirrors the challenge-targets
"inert without config" precedent), so it cannot perturb a live run until
explicitly opted in.

Schema (all optional)::

    {
      "active_node_prewarm": {
        "enabled": false,
        "diagnostics_wait_secs": 1800.0,   // generous: a normal node is minutes
        "startup_timeout_secs": 120.0,     // initialize handshake cap
        "quiet_timeout_secs": 90.0,        // target-inactivity ambiguity cap
        "giant_node_max_lines": 3000,      // skip warming a node larger than this
        "giant_node_heartbeat_ceiling": 2000000,  // ...or with a preamble
                                                 //    maxHeartbeats at/above this
        "sandbox_role": "lake_compiler",   // bwrap role for `lean --server`;
                                           //   "" disables the sandbox
        "sandbox_home": null               // bwrap HOME (supervisor workspace
                                           //   home); null => process HOME
      }
    }

SAFETY: ``sandbox_role`` defaults to ``lake_compiler`` so the server's
``lean --server`` runs under bwrap with the run repo's ``.lake/packages/<pkg>``
checkouts READ-ONLY (only ``<pkg>/.lake/build`` + ``config`` + ``widget`` are
writable). Any ``lake env`` re-clone / manifest reconciliation that tries to
rewrite a package checkout hits EROFS, so the host-side server CANNOT wipe
``.lake/packages``. Set ``sandbox_role`` to ``""`` only for an offline harness
on a throwaway repo copy; never for a server pointed at a live run.

A node classed "giant" (over ``giant_node_max_lines``, or a preamble
``maxHeartbeats`` at/above ``giant_node_heartbeat_ceiling``) is NOT warmed: the
supervisor prewarm skips it and the worker client falls straight to
``lake build``. Warming such a monolith is unreliable (async-ON cold open is
~15 min, a warm mid-edit can crash) and wastes time; the fix for giants is
splitting them.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping, Optional


# A cold open of the deliberately-huge single-theorem Tablet nodes can take
# many minutes; the prewarm pays exactly that, so the wait must be generous.
DEFAULT_DIAGNOSTICS_WAIT_SECS = 1800.0
DEFAULT_STARTUP_TIMEOUT_SECS = 120.0
DEFAULT_QUIET_TIMEOUT_SECS = 90.0
# A node larger than this many lines is too big to warm reliably (see module
# docstring); the prewarm skips it and the worker falls back to lake build.
DEFAULT_GIANT_NODE_MAX_LINES = 3000
# ...or a preamble `maxHeartbeats` at/above this ceiling marks a giant node.
DEFAULT_GIANT_NODE_HEARTBEAT_CEILING = 2_000_000
# bwrap role for the server's `lean --server`. `lake_compiler` ro-binds
# `.lake/packages/<pkg>` so the server can never wipe it (Gap-1 wipe-safety).
# An empty string disables the sandbox (offline harness on a throwaway copy
# ONLY).
DEFAULT_SANDBOX_ROLE = "lake_compiler"


@dataclass(frozen=True)
class ActiveNodePrewarmConfig:
    enabled: bool = False
    diagnostics_wait_secs: float = DEFAULT_DIAGNOSTICS_WAIT_SECS
    startup_timeout_secs: float = DEFAULT_STARTUP_TIMEOUT_SECS
    quiet_timeout_secs: float = DEFAULT_QUIET_TIMEOUT_SECS
    giant_node_max_lines: int = DEFAULT_GIANT_NODE_MAX_LINES
    giant_node_heartbeat_ceiling: int = DEFAULT_GIANT_NODE_HEARTBEAT_CEILING
    # bwrap confinement of `lean --server` (Gap-1 wipe-safety). Empty => off.
    sandbox_role: str = DEFAULT_SANDBOX_ROLE
    # bwrap HOME for the confined server (supervisor workspace home). None =>
    # the launching process HOME.
    sandbox_home: Optional[str] = None

    @staticmethod
    def from_mapping(raw: Mapping[str, Any]) -> "ActiveNodePrewarmConfig":
        def _float(key: str, default: float) -> float:
            val = raw.get(key, default)
            try:
                out = float(val)
            except (TypeError, ValueError):
                return default
            return out if out > 0 else default

        def _int(key: str, default: int) -> int:
            val = raw.get(key, default)
            try:
                out = int(val)
            except (TypeError, ValueError):
                return default
            # A non-positive value disables that giant signal; keep it as-is so
            # an operator can explicitly turn one off (the heuristic treats
            # max_lines<=0 / ceiling<1 as "do not trigger").
            return out

        return ActiveNodePrewarmConfig(
            enabled=bool(raw.get("enabled", False)),
            diagnostics_wait_secs=_float(
                "diagnostics_wait_secs", DEFAULT_DIAGNOSTICS_WAIT_SECS
            ),
            startup_timeout_secs=_float(
                "startup_timeout_secs", DEFAULT_STARTUP_TIMEOUT_SECS
            ),
            quiet_timeout_secs=_float(
                "quiet_timeout_secs", DEFAULT_QUIET_TIMEOUT_SECS
            ),
            giant_node_max_lines=_int(
                "giant_node_max_lines", DEFAULT_GIANT_NODE_MAX_LINES
            ),
            giant_node_heartbeat_ceiling=_int(
                "giant_node_heartbeat_ceiling",
                DEFAULT_GIANT_NODE_HEARTBEAT_CEILING,
            ),
            sandbox_role=str(raw.get("sandbox_role", DEFAULT_SANDBOX_ROLE)),
            sandbox_home=(
                str(raw["sandbox_home"])
                if raw.get("sandbox_home") not in (None, "")
                else None
            ),
        )

    @staticmethod
    def load(config_path: Path) -> "ActiveNodePrewarmConfig":
        """Read the ``active_node_prewarm`` object from a ``trellis.config.json``.

        Missing file or missing object => default (disabled) config, so the
        feature stays inert without explicit opt-in.
        """
        try:
            data = json.loads(Path(config_path).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return ActiveNodePrewarmConfig()
        raw = data.get("active_node_prewarm")
        if not isinstance(raw, Mapping):
            return ActiveNodePrewarmConfig()
        return ActiveNodePrewarmConfig.from_mapping(raw)
