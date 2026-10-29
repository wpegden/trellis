"""Recognize exhausted provider allowances, separately from transient throttling."""

from __future__ import annotations

import json
import re
from datetime import datetime, timezone
from typing import Any


_EXHAUSTED = re.compile(
    r"usage_limit_reached|insufficient_quota|billing_hard_limit_reached|"
    r"credit balance is too low|insufficient (?:credits|credit balance)|"
    r"you exceeded your current quota[^\n]*(?:billing|plan)|"
    r"(?:you(?:['’]ve| have)|you) (?:hit|reached) your (?:usage )?limit|"
    r"you(?:['’]re| are) out of extra usage|"
    r"you have exhausted your capacity on this model[.\s]+your quota will reset|"
    r"(?:exceeded|reached) your (?:current )?usage limit|"
    r"(?:usage|weekly|daily|monthly|spending) (?:limit|quota|budget) "
    r"(?:has been |is )?(?:reached|exceeded|exhausted)|"
    r"(?:exceeded|exhausted) (?:your |the )?(?:daily|weekly|monthly) (?:quota|budget|limit)",
    re.IGNORECASE,
)
_ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


def budget_exhaustion_detail(*, error: str = "", output: str = "") -> str:
    """Return bounded diagnostics from a failed CLI's error surface.

    JSONL agent/tool messages are deliberately excluded: an agent discussing
    quota handling is not evidence that its provider rejected a request.
    Neither bare 429/resource_exhausted nor context/attempt limits qualify.
    """
    candidates = [error]
    for line in output.splitlines()[-80:]:
        line = _ANSI.sub("", line).strip()
        try:
            event = json.loads(line)
        except (ValueError, TypeError):
            candidates.append(line)
            continue
        if not isinstance(event, dict):
            continue
        kind = event.get("type")
        if kind in {"error", "turn.failed"}:
            candidates.append(json.dumps(event.get("error", event.get("message", "")), ensure_ascii=False))
        elif kind == "result" and event.get("is_error") is True:
            candidates.append(str(event.get("result", "")))
            candidates.append(json.dumps(event.get("errors", []), ensure_ascii=False))
    for candidate in reversed(candidates):
        match = _EXHAUSTED.search(candidate)
        if match:
            # Keep the provider's reset hint when present, without copying a
            # potentially enormous transcript into the viewer pause record.
            return candidate[max(0, match.start() - 60):match.end() + 340].strip()
    return ""


class ProviderBudgetExhausted(RuntimeError):
    """Operational stop; never a mathematical or malformed-artifact verdict."""

    def __init__(self, *, provider: str, model: str | None, role: str, detail: str) -> None:
        super().__init__(detail)
        self.pause: dict[str, Any] = {
            "kind": "provider_budget",
            "provider": provider,
            "model": model,
            "role": role,
            "reason": detail,
            "detected_at": datetime.now(timezone.utc).isoformat(),
        }
