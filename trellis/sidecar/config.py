"""Daemon-side config for the parallel-closure sidecar.

Read from ``trellis.config.json`` under the ``sidecar`` object (§3.3).
The daemon reads the ``model`` / ``budgets`` / daemon-loop keys; the
kernel reads ``enabled`` / ``apply.*`` / ``phases.*`` with its own
parser (``kernel/src/sidecar.rs``) — the two halves never share a
parse. Inert when the file/block is absent or ``enabled`` is false
(the ``active_node_prewarm`` package precedent).

Schema (all keys optional except the block itself)::

    {
      "sidecar": {
        "enabled": true,
        "model": {
          "provider": "codex",
          "name": "gpt-5.6-luna",
          "reasoning_effort": "high",
          "codex_home": ""
        },
        "budgets": {
          "attempt_wall_seconds": 5400,
          "max_iterations": 0,
          "attempt_tokens": 0,
          "compact_context_tokens": 200000,
          "daily_tokens": 0,
          "monthly_tokens": 0
        },
        "daemon": {
          "grunts": 2,
          "poll_seconds": 30,
          "export_stale_after_seconds": 7200,
          "lean_threads": 2,
          "giant_node_max_lines": 3000,
          "sandbox_role": "lake_compiler",
          "allow_unsandboxed": false
        }
      }
    }

KEY HYGIENE: the API key is read from ``api_key_env_file`` (mode-0600
env file) at request time only. It never appears in argv, config
values, logs, ledger rows, or spool records — and request HEADERS are
never logged (amendment A9); ``tests/test_sidecar_driver.py`` pins
this.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, Mapping, Optional


DEFAULT_PROVIDER = "codex"
DEFAULT_MODEL_NAME = "gpt-5.6-luna"
# Legacy HTTP-arm fields. The chat/completions attempt arm is retired
# (attempts run through the codex CLI); these remain parseable so an old
# config block still loads, and read_api_key still honours them for a
# non-codex provider.
DEFAULT_ENDPOINT = "https://api.mistral.ai/v1/chat/completions"
DEFAULT_API_KEY_ENV_FILE = "~/.vibe/.env"
DEFAULT_API_KEY_VAR = "MISTRAL_API_KEY"

# Trained-regime knob (Leanstral docs: "high" recommended for hard
# proofs). "" / "none" => the field is not sent; otherwise it reaches
# `codex exec` as `-c reasoning_effort=<value>`.
DEFAULT_REASONING_EFFORT = "high"

# WALL-CLOCK REGIME (BENCH_REPORT §7.8). A wall-clock-only budget closed
# 3/3 previously-failed nodes on free-tier Leanstral, one grinding 65 min
# / 420 steps / 3 compactions / 43.66M tokens — outcomes physically
# impossible under the earlier 16-iter / 200k-token caps, which were the
# abort-too-early bug in production form. DESIGN: wall time is the SOLE
# per-attempt budget; there is NO per-attempt token or iteration cap and
# NO cumulative-volume (daily/monthly) cap. 0 == disabled for every token
# / iteration limit below. Total system load is bounded solely by the
# grunt POOL SIZE (``grunts``, 2 by default) each running one attempt at
# a time — throughput is capped by the pool, not by any token budget.
# 90 min: the regime the evidence was collected under. The bench's
# decisive close (NestedPiInvariantEventsIndependent, BENCH_REPORT §7.8)
# took 64.9 min under a 90-min wall, where the earlier 60-min default
# would have killed it ~8% short of the finish line and reported
# budget_exhausted — the abort-too-early bug in a new form. An INITIAL
# value to observe and tune, not a permanent floor.
DEFAULT_ATTEMPT_WALL_SECONDS = 5400.0
DEFAULT_MAX_ITERATIONS = 0  # 0 == unbounded (only the wall ends an attempt)
DEFAULT_ATTEMPT_TOKENS = 0  # 0 == no per-attempt token cap
# v3 context compaction (Leanstral trained long-horizon regime): compact
# the conversation once its LIVE context window (last turn's
# prompt+completion) reaches this many tokens — vibe's
# auto_compact_threshold analog. The §7.8 3/3 wall-clock run used vibe's
# native 200k; with no billed cap in play the earlier economics-shaped
# 30-60k lowering no longer applies. 0 disables compaction.
DEFAULT_COMPACT_CONTEXT_TOKENS = 200_000
# Ledger volume caps are DISABLED (0). The ledger stays a RECORDER only
# (per-attempt token/wall telemetry for observability); it never GATES a
# new attempt. Nothing about cumulative token volume may pause, idle,
# suspend, or wedge the daemon.
DEFAULT_DAILY_TOKENS = 0
DEFAULT_MONTHLY_TOKENS = 0

DEFAULT_POLL_SECONDS = 30.0
DEFAULT_EXPORT_STALE_AFTER_SECONDS = 7200.0
DEFAULT_GRUNTS = 2
DEFAULT_LEAN_THREADS = 2
DEFAULT_GIANT_NODE_MAX_LINES = 3000
DEFAULT_SANDBOX_ROLE = "lake_compiler"


@dataclass(frozen=True)
class SidecarConfig:
    enabled: bool = False
    # -- model (model-agnostic OpenAI-compatible endpoint) ---------------
    provider: str = DEFAULT_PROVIDER
    model_name: str = DEFAULT_MODEL_NAME
    endpoint: str = DEFAULT_ENDPOINT
    # `provider: "codex"` drives the codex CLI (subscription auth) instead
    # of an HTTP chat/completions endpoint; `codex_home` isolates its state
    # from the live run's own codex sessions.
    codex_home: str = ""
    api_key_env_file: str = DEFAULT_API_KEY_ENV_FILE
    api_key_var: str = DEFAULT_API_KEY_VAR
    tool_calls: bool = True
    # Trained-regime effort ("high" default; "" / "none" => the field is
    # not sent).
    reasoning_effort: str = DEFAULT_REASONING_EFFORT
    # -- budgets (E§4.3; wall-clock regime) -------------------------------
    # Wall is the sole per-attempt budget. ``max_iterations`` /
    # ``attempt_tokens`` == 0 mean "no cap" (only the wall ends an
    # attempt). ``daily_tokens`` / ``monthly_tokens`` == 0 mean the ledger
    # never gates on cumulative volume (it only records).
    attempt_wall_seconds: float = DEFAULT_ATTEMPT_WALL_SECONDS
    max_iterations: int = DEFAULT_MAX_ITERATIONS
    attempt_tokens: int = DEFAULT_ATTEMPT_TOKENS
    # v3 compaction threshold on live context tokens (0 disables).
    compact_context_tokens: int = DEFAULT_COMPACT_CONTEXT_TOKENS
    daily_tokens: int = DEFAULT_DAILY_TOKENS
    monthly_tokens: int = DEFAULT_MONTHLY_TOKENS
    # -- daemon loop -------------------------------------------------------
    # Grunt pool size (owner decision 2): one isolated workspace each.
    # Retry policy knobs are deliberately ABSENT — retry is entirely the
    # reviewer's (queue redesign Q5/Q9: one attempt per queue-entry
    # generation; retry = remove + re-add).
    grunts: int = DEFAULT_GRUNTS
    poll_seconds: float = DEFAULT_POLL_SECONDS
    export_stale_after_seconds: float = DEFAULT_EXPORT_STALE_AFTER_SECONDS
    lean_threads: int = DEFAULT_LEAN_THREADS
    giant_node_max_lines: int = DEFAULT_GIANT_NODE_MAX_LINES
    sandbox_role: str = DEFAULT_SANDBOX_ROLE
    # F5 escape hatch: with a sandbox role configured but bwrap absent,
    # the daemon REFUSES to run compile steps unless this is explicitly
    # true (and then logs loudly). Never set on a live run.
    allow_unsandboxed: bool = False
    # -- host services -----------------------------------------------------
    # Whether a local Loogle server is configured for this project.
    # Read from the TOP-LEVEL ``loogle.enabled`` key of
    # ``trellis.config.json`` (the worker-prompt precedent in
    # ``runtime/bridge_prompts.py``), not the sidecar block; default
    # True when absent, matching that precedent. False ⇒ the driver's
    # ``search_mathlib`` tool is absent (never advertised-but-broken).
    loogle_enabled: bool = True

    def resolved_api_key_env_file(self) -> Path:
        return Path(self.api_key_env_file).expanduser()

    @staticmethod
    def from_mapping(raw: Mapping[str, Any]) -> "SidecarConfig":
        def _sub(key: str) -> Mapping[str, Any]:
            val = raw.get(key)
            return val if isinstance(val, Mapping) else {}

        model = _sub("model")
        budgets = _sub("budgets")
        daemon = _sub("daemon")

        def _float(block: Mapping[str, Any], key: str, default: float) -> float:
            try:
                out = float(block.get(key, default))
            except (TypeError, ValueError):
                return default
            return out if out > 0 else default

        def _int(block: Mapping[str, Any], key: str, default: int) -> int:
            try:
                out = int(block.get(key, default))
            except (TypeError, ValueError):
                return default
            return out if out > 0 else default

        def _int0(block: Mapping[str, Any], key: str, default: int) -> int:
            """Like _int but 0 is a meaningful value (cap/feature off).
            Negative => fall back to the default."""
            try:
                out = int(block.get(key, default))
            except (TypeError, ValueError):
                return default
            return out if out >= 0 else default

        return SidecarConfig(
            enabled=bool(raw.get("enabled", False)),
            provider=str(model.get("provider", DEFAULT_PROVIDER)),
            model_name=str(model.get("name", DEFAULT_MODEL_NAME)),
            endpoint=str(model.get("endpoint", DEFAULT_ENDPOINT)),
            codex_home=str(model.get("codex_home", "") or ""),
            api_key_env_file=str(
                model.get("api_key_env_file", DEFAULT_API_KEY_ENV_FILE)
            ),
            api_key_var=str(model.get("api_key_var", DEFAULT_API_KEY_VAR)),
            tool_calls=bool(model.get("tool_calls", True)),
            reasoning_effort=str(
                model.get("reasoning_effort", DEFAULT_REASONING_EFFORT) or ""
            ),
            attempt_wall_seconds=_float(
                budgets, "attempt_wall_seconds", DEFAULT_ATTEMPT_WALL_SECONDS
            ),
            max_iterations=_int0(budgets, "max_iterations", DEFAULT_MAX_ITERATIONS),
            attempt_tokens=_int0(budgets, "attempt_tokens", DEFAULT_ATTEMPT_TOKENS),
            compact_context_tokens=_int0(
                budgets, "compact_context_tokens", DEFAULT_COMPACT_CONTEXT_TOKENS
            ),
            daily_tokens=_int0(budgets, "daily_tokens", DEFAULT_DAILY_TOKENS),
            monthly_tokens=_int0(budgets, "monthly_tokens", DEFAULT_MONTHLY_TOKENS),
            grunts=_int(daemon, "grunts", DEFAULT_GRUNTS),
            poll_seconds=_float(daemon, "poll_seconds", DEFAULT_POLL_SECONDS),
            export_stale_after_seconds=_float(
                daemon,
                "export_stale_after_seconds",
                DEFAULT_EXPORT_STALE_AFTER_SECONDS,
            ),
            lean_threads=_int(daemon, "lean_threads", DEFAULT_LEAN_THREADS),
            giant_node_max_lines=_int(
                daemon, "giant_node_max_lines", DEFAULT_GIANT_NODE_MAX_LINES
            ),
            sandbox_role=str(daemon.get("sandbox_role", DEFAULT_SANDBOX_ROLE)),
            allow_unsandboxed=bool(daemon.get("allow_unsandboxed", False)),
        )

    @staticmethod
    def load(config_path: Path) -> "SidecarConfig":
        """Read the ``sidecar`` object from ``trellis.config.json``.

        Missing file, invalid JSON, or missing/non-mapping block =>
        default (disabled) config — inert without explicit opt-in.
        """
        try:
            data = json.loads(Path(config_path).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return SidecarConfig()
        raw = data.get("sidecar")
        if not isinstance(raw, Mapping):
            return SidecarConfig()
        config = SidecarConfig.from_mapping(raw)
        # Host-service knob shared with the worker prompt: TOP-LEVEL
        # ``loogle.enabled`` (default True when absent).
        loogle = data.get("loogle")
        if isinstance(loogle, Mapping) and not bool(loogle.get("enabled", True)):
            config = replace(config, loogle_enabled=False)
        return config


def uses_codex_cli(config: SidecarConfig) -> bool:
    """True when attempts run through the codex CLI rather than HTTP."""
    return (config.provider or "").strip().lower() == "codex"


def read_api_key(config: SidecarConfig) -> Optional[str]:
    """Read the API key from the configured env file (never argv/env).

    Returns None (and the caller suspends) when the file or variable is
    absent. The value must NEVER be logged or embedded in any record.

    A codex-CLI provider has no API key to read — it authenticates from
    CODEX_HOME — so it returns a sentinel rather than None, which would
    otherwise suspend the daemon on a gate that does not apply to it.
    """
    if uses_codex_cli(config):
        return "codex-cli"
    path = config.resolved_api_key_env_file()
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return None
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        if key.strip() == config.api_key_var:
            value = value.strip().strip('"').strip("'")
            return value or None
    return None
