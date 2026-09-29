"""Shared agent-wrapper layer for the active trellis runtime."""

from .executor import (
    ArtifactPaths,
    ArtifactPromotionResult,
    DefaultLanePortResolver,
    execute_agent_request,
    prepare_artifact_paths,
    validate_and_promote_artifact,
)
from .protocol import (
    AgentLane,
    ArtifactSpec,
    PanelRequest,
    PanelResponse,
    SingleAgentRequest,
    SingleAgentResponse,
)

__all__ = [
    "AgentLane",
    "ArtifactPaths",
    "ArtifactPromotionResult",
    "ArtifactSpec",
    "DefaultLanePortResolver",
    "PanelRequest",
    "PanelResponse",
    "SingleAgentRequest",
    "SingleAgentResponse",
    "execute_agent_request",
    "prepare_artifact_paths",
    "validate_and_promote_artifact",
]
