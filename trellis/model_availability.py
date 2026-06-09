"""Track model availability for fallback on capacity exhaustion.

When a model returns 429 MODEL_CAPACITY_EXHAUSTED, we mark it
unavailable for a cooldown period and try the next model in the
fallback list. After the cooldown expires, we're willing to try again.

Module-level singleton — no persistence needed since capacity issues
are transient and state resets on process restart.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional


@dataclass
class ModelBlock:
    """A temporarily blocked model."""
    model: str
    blocked_at: float  # time.monotonic()
    reason: str


class ModelAvailability:
    """Tracks which models are temporarily unavailable."""

    def __init__(self, cooldown_seconds: float = 300.0):
        self._blocks: Dict[str, ModelBlock] = {}
        self.cooldown = cooldown_seconds

    def mark_unavailable(self, model: str, reason: str = "") -> None:
        """Mark a model as temporarily unavailable."""
        self._blocks[model] = ModelBlock(
            model=model,
            blocked_at=time.monotonic(),
            reason=reason,
        )
        print(f"  Model {model} marked unavailable: {reason}")

    def is_available(self, model: str) -> bool:
        """Check if a model is available (not blocked, or cooldown expired)."""
        block = self._blocks.get(model)
        if block is None:
            return True
        elapsed = time.monotonic() - block.blocked_at
        if elapsed >= self.cooldown:
            del self._blocks[model]
            return True
        return False

    def pick_available(self, candidates: List[str]) -> Optional[str]:
        """Return the first available model from candidates, or None."""
        for model in candidates:
            if self.is_available(model):
                return model
        return None

    def status(self) -> Dict[str, Any]:
        """Return current block status for logging."""
        now = time.monotonic()
        return {
            model: {
                "reason": block.reason,
                "blocked_seconds_ago": round(now - block.blocked_at, 1),
                "cooldown_remaining": round(max(0, self.cooldown - (now - block.blocked_at)), 1),
            }
            for model, block in self._blocks.items()
        }


# Module-level singleton
_global_availability = ModelAvailability()


def get_availability() -> ModelAvailability:
    """Get the global model availability tracker."""
    return _global_availability
