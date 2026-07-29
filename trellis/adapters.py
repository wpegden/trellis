"""Provider adapter data types and configuration.

This module defines the data types for provider configuration, burst results,
and usage snapshots. Execution logic lives in burst.py.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional


@dataclass
class ProviderConfig:
    """Configuration for a single provider/role."""
    provider: str  # "claude", "codex", "gemini"
    model: Optional[str] = None
    effort: Optional[str] = None  # claude: low/medium/high/max
    extra_args: List[str] = field(default_factory=list)
    fallback_models: List[str] = field(default_factory=list)  # ordered most→least powerful


@dataclass
class BurstResult:
    """Result of a single agent burst."""
    ok: bool
    exit_code: Optional[int]
    captured_output: str
    duration_seconds: float
    stall_recoveries: int = 0
    usage: Optional[Dict[str, Any]] = None
    error: str = ""
    recovery_log: List[str] = field(default_factory=list)
    transcript_path: Optional[Path] = None  # path to saved agent chat transcript


@dataclass
class UsageSnapshot:
    """Token/cost usage from a single burst."""
    input_tokens: int = 0
    output_tokens: int = 0
    cached_input_tokens: int = 0
    reasoning_tokens: int = 0
    total_tokens: int = 0
    cost_usd: Optional[float] = None
    raw: Optional[Dict[str, Any]] = None
