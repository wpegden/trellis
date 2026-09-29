"""Provider content-policy refusals.

Codex ends a turn with a structured ``error`` / ``turn.failed`` record when
the provider's cybersecurity classifier rejects the request ("This content
was flagged for possible cybersecurity risk ..."). The rejection is decided
on the accumulated conversation, so it typically lands minutes into a burst,
after the agent has already run tools. It is neither a rate limit nor a
crash: a refused request stops for operator review rather than being routed
to another model.

Detection reads only structured provider records. Tool output that merely
quotes the phrase (an agent grepping an old rollout) is not a refusal.
"""

from __future__ import annotations

import json

POLICY_REFUSAL_PATTERNS: tuple[str, ...] = (
    "flagged for possible cybersecurity risk",
    "cyber_policy",
)

# Prefix the burst layer puts on ``BurstResult.error`` when the provider
# refuses; the bridge keys its circuit breaker on it.
POLICY_REFUSAL_MARKER = "policy_refused"


def provider_error_messages(output: str) -> list[str]:
    """Provider failure messages from structured stream records, not agent/tool prose."""
    messages: list[str] = []
    for line in output.splitlines():
        try:
            record = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            continue
        if not isinstance(record, dict):
            continue
        if record.get("type") not in {"error", "turn.failed"}:
            continue
        error = record.get("error")
        nested = error.get("message") if isinstance(error, dict) else None
        for message in (record.get("message"), nested):
            if isinstance(message, str) and message.strip():
                messages.append(message.strip())
    return messages


# A fallback rung the current account cannot run answers with a structured
# 4xx naming the model (Codex: "The '<model>' model is not supported when using
# Codex with a ChatGPT account."). This is classified separately
# from a safety refusal or account-budget exhaustion.
UNAVAILABLE_MODEL_PATTERNS: tuple[str, ...] = (
    "model is not supported",
    "is not supported when using codex",
    "model_not_found",
    "unknown model",
    "no such model",
    "does not exist or you do not have access",
)


def is_unavailable_model_message(message: str) -> bool:
    lowered = message.lower()
    return any(pattern in lowered for pattern in UNAVAILABLE_MODEL_PATTERNS)


def is_unavailable_model(output: str, error: str = "") -> bool:
    """True when the provider rejected the request because the selected model
    is not available to this account, read from structured records or the
    backend's error summary."""
    if any(is_unavailable_model_message(m) for m in provider_error_messages(output)):
        return True
    return bool(error) and is_unavailable_model_message(error)


def is_policy_refusal_message(message: str) -> bool:
    lowered = message.lower()
    return any(pattern in lowered for pattern in POLICY_REFUSAL_PATTERNS)


def is_policy_refusal(output: str, error: str = "") -> bool:
    """True when the burst's structured provider records, or the backend's
    own error summary of them, carry a content-policy refusal."""
    if any(is_policy_refusal_message(m) for m in provider_error_messages(output)):
        return True
    return bool(error) and is_policy_refusal_message(error)
