"""Off-by-default config for the warm-prefix Isabelle checker session (Phase 1).

The warm-prefix capability lets the checker's already-warm ``IsabelleSession``
hold accepted sibling ``Tablet_<Node>.thy`` theories WARM in a held-open
``isabelle server`` document (Phase-0 "Option A"), so a later node re-check
re-elaborates only the in-flight theory against the warm prefix instead of
re-elaborating the whole node graph from source each call.

This config object is the supervisor-level loader; it mirrors
:class:`trellis.active_node_prewarm.config.ActiveNodePrewarmConfig` exactly
(the Lean warm-server seam): the whole capability is INERT unless ``enabled``
is true, so it cannot perturb a live run until explicitly opted in. Read from
``trellis.config.json`` under an ``isabelle_warm_session`` object.

Schema (all optional)::

    {
      "isabelle_warm_session": {
        "enabled": false,
        "consolidate_delay": 0.05,     // headless_consolidate_delay at
                                       //   session_start; Phase-0 tuned ~0.05
                                       //   → sub-0.5s node re-checks. Only
                                       //   applied when enabled is true.
        "cross_check_cadence": 25      // Phase 2 cold-rebuild backstop: every
                                       //   Nth soundness (thm-deps) warm check
                                       //   ALSO runs a cold build + cert and
                                       //   HALTS on any warm-vs-cold mismatch.
                                       //   0 disables the periodic cadence (the
                                       //   anomaly→cold fallback + a forced
                                       //   cross-check still apply); a forced
                                       //   cross-check is requested out-of-band
                                       //   via TRELLIS_ISABELLE_WARM_FORCE_COLD_
                                       //   CROSSCHECK (e.g. at tablet completion).
      }
    }

SOUNDNESS NOTE: turning this on changes only HOW FAST the kernel ``thm`` value
is produced (warm prefix vs cold re-elaboration), never WHAT it is — the
soundness certificate (oracles / deps / shyps / statement) is a pure function
of the elaborated theorem, and Phase 0 verified the warm cert is byte-identical
to a cold build of the same accepted source. The off-by-default flag exists so
this is opted into deliberately (and wired into the node-check gate in a later
phase), not because warm ≠ cold.

There is also an env fast-path the :class:`IsabelleSession` itself reads
(``TRELLIS_ISABELLE_WARM_SESSION``), mirroring the other ``TRELLIS_ISABELLE_*``
env overrides the session already honors (``TRELLIS_ISABELLE_BIN`` /
``TRELLIS_ISABELLE_BASE_SESSION``). The dataclass is the structured loader the
supervisor uses to thread the flag in; the env var is the low-level switch the
session reads when no explicit value is passed. Both default OFF.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping, Optional

# The env switch the low-level ``IsabelleSession`` reads when its
# ``warm_prefix_enabled`` argument is left unset. Matches the existing
# ``TRELLIS_ISABELLE_*`` override convention. Default OFF (any value other
# than the truthy set below is OFF).
WARM_SESSION_ENV = "TRELLIS_ISABELLE_WARM_SESSION"

# Phase-2 cold-rebuild backstop cadence override (the structured
# ``cross_check_cadence`` config field's env twin). Every Nth soundness warm
# check ALSO runs a cold build + cert and HALTS on any warm-vs-cold mismatch.
# An unset/empty/non-int value leaves the config-file value in force; ``0``
# disables the periodic cadence (the anomaly→cold fallback still applies).
CROSS_CHECK_ENV = "TRELLIS_ISABELLE_WARM_CROSS_CHECK_EVERY"

# Phase-2 FORCED cross-check switch. When truthy, the NEXT soundness warm
# check runs a cold cross-check regardless of the cadence counter — the
# "mandatory at tablet completion" hook (the checker server is stateless and
# has no completion event, so the operator/supervisor sets this to force one).
FORCE_CROSS_CHECK_ENV = "TRELLIS_ISABELLE_WARM_FORCE_COLD_CROSSCHECK"

# Phase-4 hardening H2: the consecutive-cold-channel-failure alarm threshold's
# env twin. After this many cross-checks skipped in a row for a cold-channel
# failure, the gate surfaces a loud non-halting alarm. Unset/empty/unparseable
# leaves the config-file value in force; ``0`` disables the alarm.
COLD_FAILURE_ALARM_ENV = "TRELLIS_ISABELLE_WARM_COLD_FAILURE_ALARM_AFTER"

# Default periodic cadence: every 25th soundness warm check runs a cold
# cross-check. Conservative defence-in-depth (Phase-0 proved warm≡cold, so a
# mismatch is a heap-corruption alarm, not an expected event); cheap because
# ``Tablet_Base`` is prebuilt. Tunable down once a real run measures the cost.
DEFAULT_CROSS_CHECK_CADENCE = 25

# Phase-4 hardening H2: after this many CONSECUTIVE cold cross-checks that could
# not run because the cold CHANNEL failed (cold session/build spawn or timeout),
# the gate surfaces a loud, NON-halting alarm (log + a distinct
# ``checker_cold_crosscheck_degraded.json`` marker) so a chronically-flaky cold
# channel — which silently erodes the cross-check tier — is visible rather than
# unnoticed. The SPINE cold build (run by the kernel per cert op) still fails
# CLOSED on a wedged channel, so this is visibility, not a soundness hole. ``0``
# disables the alarm. Conservative default so a transient blip does not alarm.
DEFAULT_COLD_FAILURE_ALARM_AFTER = 3

# Phase-0 settled knob: ``headless_consolidate_delay`` at ``session_start``.
# 2.0 (the Isabelle default) is the source of the ~2s PIDE floor; ~0.05
# collapses a node re-check to sub-0.5s with a still-sound verdict. A small
# positive value is used over 0.0 (0.0 risks a premature-FAILED false-fast).
DEFAULT_CONSOLIDATE_DELAY = 0.05

# Accepted truthy spellings for the env switch (case-insensitive).
_TRUTHY = frozenset({"1", "true", "yes", "on"})


def env_warm_session_enabled() -> bool:
    """Resolve the warm-prefix env switch (default False).

    ``$TRELLIS_ISABELLE_WARM_SESSION`` in ``{1,true,yes,on}`` (case-folded) →
    True; anything else (including unset/empty) → False. This is the
    low-level switch :class:`IsabelleSession` consults when its
    ``warm_prefix_enabled`` constructor argument is left unset.
    """
    raw = os.environ.get(WARM_SESSION_ENV, "").strip().lower()
    return raw in _TRUTHY


def env_cross_check_cadence() -> Optional[int]:
    """Resolve the cold-cross-check cadence env override (default: unset).

    ``$TRELLIS_ISABELLE_WARM_CROSS_CHECK_EVERY`` parsed as a non-negative int
    (``0`` = disable the periodic cadence). Returns ``None`` when unset/empty/
    unparseable/negative so the structured config-file value stays in force.
    """
    raw = os.environ.get(CROSS_CHECK_ENV, "").strip()
    if not raw:
        return None
    try:
        value = int(raw)
    except ValueError:
        return None
    return value if value >= 0 else None


def env_force_cross_check() -> bool:
    """Resolve the FORCED-cross-check switch (default False).

    ``$TRELLIS_ISABELLE_WARM_FORCE_COLD_CROSSCHECK`` in ``{1,true,yes,on}`` →
    True. Forces a cold cross-check on the next soundness warm check regardless
    of the cadence counter (the stateless checker's stand-in for a
    "mandatory at tablet completion" trigger).
    """
    raw = os.environ.get(FORCE_CROSS_CHECK_ENV, "").strip().lower()
    return raw in _TRUTHY


def env_cold_failure_alarm_after() -> Optional[int]:
    """Resolve the consecutive-cold-failure alarm threshold env override.

    ``$TRELLIS_ISABELLE_WARM_COLD_FAILURE_ALARM_AFTER`` parsed as a non-negative
    int (``0`` = disable the alarm). Returns ``None`` when unset/empty/
    unparseable/negative so the structured config-file value stays in force.
    """
    raw = os.environ.get(COLD_FAILURE_ALARM_ENV, "").strip()
    if not raw:
        return None
    try:
        value = int(raw)
    except ValueError:
        return None
    return value if value >= 0 else None


@dataclass(frozen=True)
class IsabelleWarmSessionConfig:
    """Structured warm-prefix config (supervisor-level loader).

    ``enabled`` defaults False so the capability is inert without opt-in.
    ``consolidate_delay`` is the Phase-0 ``headless_consolidate_delay`` knob;
    it is only meaningful (and only applied) when ``enabled`` is true.
    ``cross_check_cadence`` is the Phase-2 cold-rebuild backstop: every Nth
    soundness warm check ALSO runs a cold build + cert and HALTS on any
    warm-vs-cold mismatch (``0`` disables the periodic cadence, but the
    anomaly→cold fallback and a forced cross-check still apply).
    """

    enabled: bool = False
    consolidate_delay: float = DEFAULT_CONSOLIDATE_DELAY
    cross_check_cadence: int = DEFAULT_CROSS_CHECK_CADENCE
    cold_failure_alarm_after: int = DEFAULT_COLD_FAILURE_ALARM_AFTER

    @staticmethod
    def from_mapping(raw: Mapping[str, Any]) -> "IsabelleWarmSessionConfig":
        def _delay() -> float:
            val = raw.get("consolidate_delay", DEFAULT_CONSOLIDATE_DELAY)
            try:
                out = float(val)
            except (TypeError, ValueError):
                return DEFAULT_CONSOLIDATE_DELAY
            # A non-positive delay is rejected (0.0 risks a premature-FAILED
            # false-fast, negatives are nonsense); fall back to the tuned
            # default rather than honoring an unsafe value.
            return out if out > 0 else DEFAULT_CONSOLIDATE_DELAY

        def _cadence() -> int:
            val = raw.get("cross_check_cadence", DEFAULT_CROSS_CHECK_CADENCE)
            try:
                out = int(val)
            except (TypeError, ValueError):
                return DEFAULT_CROSS_CHECK_CADENCE
            # A negative cadence is nonsense → the tuned default. ``0`` is a
            # legitimate "disable the periodic cadence" value, kept verbatim.
            return out if out >= 0 else DEFAULT_CROSS_CHECK_CADENCE

        def _alarm() -> int:
            val = raw.get(
                "cold_failure_alarm_after", DEFAULT_COLD_FAILURE_ALARM_AFTER
            )
            try:
                out = int(val)
            except (TypeError, ValueError):
                return DEFAULT_COLD_FAILURE_ALARM_AFTER
            # ``0`` legitimately disables the alarm; negatives are nonsense.
            return out if out >= 0 else DEFAULT_COLD_FAILURE_ALARM_AFTER

        return IsabelleWarmSessionConfig(
            enabled=bool(raw.get("enabled", False)),
            consolidate_delay=_delay(),
            cross_check_cadence=_cadence(),
            cold_failure_alarm_after=_alarm(),
        )

    def effective_cross_check_cadence(self) -> int:
        """The cadence in force, env override winning over the config value."""
        override = env_cross_check_cadence()
        return self.cross_check_cadence if override is None else override

    def effective_cold_failure_alarm_after(self) -> int:
        """The H2 alarm threshold in force (env override winning)."""
        override = env_cold_failure_alarm_after()
        return self.cold_failure_alarm_after if override is None else override

    @staticmethod
    def load(config_path: Path) -> "IsabelleWarmSessionConfig":
        """Read the ``isabelle_warm_session`` object from a ``trellis.config.json``.

        Missing file or missing object => default (disabled) config, so the
        capability stays inert without an explicit opt-in.
        """
        try:
            data = json.loads(Path(config_path).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return IsabelleWarmSessionConfig()
        raw = data.get("isabelle_warm_session")
        if not isinstance(raw, Mapping):
            return IsabelleWarmSessionConfig()
        return IsabelleWarmSessionConfig.from_mapping(raw)


__all__ = [
    "WARM_SESSION_ENV",
    "CROSS_CHECK_ENV",
    "FORCE_CROSS_CHECK_ENV",
    "COLD_FAILURE_ALARM_ENV",
    "DEFAULT_CONSOLIDATE_DELAY",
    "DEFAULT_CROSS_CHECK_CADENCE",
    "DEFAULT_COLD_FAILURE_ALARM_AFTER",
    "env_warm_session_enabled",
    "env_cross_check_cadence",
    "env_force_cross_check",
    "env_cold_failure_alarm_after",
    "IsabelleWarmSessionConfig",
]
