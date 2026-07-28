"""Protocol types for the shared agent wrapper."""

from __future__ import annotations

from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional

from trellis.adapters import ProviderConfig
from trellis.config import SandboxConfig


@dataclass(frozen=True)
class AgentLane:
    """Logical execution lane understood by the wrapper, not the supervisor."""

    kind: str
    agent_index: int = 0
    node_name: str = ""
    node_index: int = 0

    def key(self) -> str:
        parts = [self.kind, str(self.agent_index)]
        if self.node_name:
            parts.append(self.node_name)
        if self.node_index:
            parts.append(str(self.node_index))
        return ":".join(parts)

    def to_dict(self) -> Dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "AgentLane":
        return cls(
            kind=str(data.get("kind", "") or ""),
            agent_index=int(data.get("agent_index", 0) or 0),
            node_name=str(data.get("node_name", "") or ""),
            node_index=int(data.get("node_index", 0) or 0),
        )


@dataclass(frozen=True)
class ArtifactSpec:
    """Validated artifact contract for one wrapper request."""

    canonical_name: str
    kind: str
    phase: Optional[str] = None
    node_name: Optional[str] = None
    invalid_attempt: bool = False

    def to_dict(self) -> Dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "ArtifactSpec":
        return cls(
            canonical_name=str(data.get("canonical_name", "") or ""),
            kind=str(data.get("kind", "") or ""),
            phase=(str(data.get("phase", "") or "").strip() or None),
            node_name=(str(data.get("node_name", "") or "").strip() or None),
            invalid_attempt=bool(data.get("invalid_attempt", False)),
        )


@dataclass(frozen=True)
class SingleAgentRequest:
    """One logical wrapper request executed against exactly one agent."""

    request_id: str
    cycle: int
    kind: str
    burst_role: str
    provider: ProviderConfig
    prompt: str
    work_dir: Path
    state_dir: Path
    session_name: str
    lane: AgentLane
    timeout_seconds: float
    session_scope: str = ""
    startup_timeout_seconds: float = 3600.0
    burst_home: Optional[Path] = None
    log_dir: Optional[Path] = None
    fresh: bool = False
    artifact: Optional[ArtifactSpec] = None
    artifact_prefix: Optional[str] = None
    sandbox: Optional[SandboxConfig] = None

    def to_dict(self) -> Dict[str, Any]:
        data = asdict(self)
        data["provider"] = asdict(self.provider)
        data["work_dir"] = str(self.work_dir)
        data["state_dir"] = str(self.state_dir)
        data["burst_home"] = str(self.burst_home) if self.burst_home is not None else None
        data["log_dir"] = str(self.log_dir) if self.log_dir is not None else None
        data["artifact"] = self.artifact.to_dict() if self.artifact is not None else None
        data["lane"] = self.lane.to_dict()
        if self.sandbox is not None:
            data["sandbox"] = asdict(self.sandbox)
        return data

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "SingleAgentRequest":
        provider_raw = data.get("provider", {})
        lane_raw = data.get("lane", {})
        artifact_raw = data.get("artifact", None)
        sandbox_raw = data.get("sandbox", None)
        return cls(
            request_id=str(data.get("request_id", "") or ""),
            cycle=int(data.get("cycle", 0) or 0),
            kind=str(data.get("kind", "") or ""),
            burst_role=str(data.get("burst_role", "") or ""),
            provider=ProviderConfig(**provider_raw),
            prompt=str(data.get("prompt", "") or ""),
            work_dir=Path(str(data.get("work_dir", ""))),
            state_dir=Path(str(data.get("state_dir", ""))),
            session_name=str(data.get("session_name", "") or ""),
            session_scope=str(data.get("session_scope", "") or ""),
            lane=AgentLane.from_dict(lane_raw),
            timeout_seconds=float(data.get("timeout_seconds", 0.0) or 0.0),
            startup_timeout_seconds=float(data.get("startup_timeout_seconds", 3600.0) or 3600.0),
            burst_home=Path(str(data.get("burst_home"))) if data.get("burst_home") else None,
            log_dir=Path(str(data.get("log_dir"))) if data.get("log_dir") else None,
            fresh=bool(data.get("fresh", False)),
            artifact=ArtifactSpec.from_dict(artifact_raw) if isinstance(artifact_raw, dict) else None,
            artifact_prefix=(str(data.get("artifact_prefix", "") or "").strip() or None),
            sandbox=SandboxConfig(**sandbox_raw) if isinstance(sandbox_raw, dict) else None,
        )


@dataclass(frozen=True)
class SingleAgentResponse:
    """Normalized wrapper response for one executed request."""

    request_id: str
    cycle: int
    kind: str
    burst_role: str
    ok: bool
    payload: Optional[Dict[str, Any]] = None
    error: str = ""
    comments: str = ""
    usage: Optional[Dict[str, Any]] = None
    captured_output: str = ""
    exit_code: Optional[int] = None
    stall_recoveries: int = 0
    transcript_path: Optional[Path] = None
    walltime_seconds: float = 0.0
    canonical_path: Optional[Path] = None
    raw_path: Optional[Path] = None
    done_path: Optional[Path] = None

    def to_dict(self) -> Dict[str, Any]:
        return {
            "request_id": self.request_id,
            "cycle": self.cycle,
            "kind": self.kind,
            "burst_role": self.burst_role,
            "ok": self.ok,
            "payload": self.payload,
            "error": self.error,
            "comments": self.comments,
            "usage": self.usage,
            "captured_output": self.captured_output,
            "exit_code": self.exit_code,
            "stall_recoveries": self.stall_recoveries,
            "transcript_path": str(self.transcript_path) if self.transcript_path is not None else None,
            "walltime_seconds": self.walltime_seconds,
            "canonical_path": str(self.canonical_path) if self.canonical_path is not None else None,
            "raw_path": str(self.raw_path) if self.raw_path is not None else None,
            "done_path": str(self.done_path) if self.done_path is not None else None,
        }

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "SingleAgentResponse":
        return cls(
            request_id=str(data.get("request_id", "") or ""),
            cycle=int(data.get("cycle", 0) or 0),
            kind=str(data.get("kind", "") or ""),
            burst_role=str(data.get("burst_role", "") or ""),
            ok=bool(data.get("ok", False)),
            payload=data.get("payload") if isinstance(data.get("payload"), dict) else None,
            error=str(data.get("error", "") or ""),
            comments=str(data.get("comments", data.get("feedback", "")) or ""),
            usage=data.get("usage") if isinstance(data.get("usage"), dict) else None,
            captured_output=str(data.get("captured_output", "") or ""),
            exit_code=(int(data["exit_code"]) if data.get("exit_code") is not None else None),
            stall_recoveries=int(data.get("stall_recoveries", 0) or 0),
            transcript_path=Path(str(data.get("transcript_path"))) if data.get("transcript_path") else None,
            walltime_seconds=float(data.get("walltime_seconds", 0.0) or 0.0),
            canonical_path=Path(str(data.get("canonical_path"))) if data.get("canonical_path") else None,
            raw_path=Path(str(data.get("raw_path"))) if data.get("raw_path") else None,
            done_path=Path(str(data.get("done_path"))) if data.get("done_path") else None,
        )


@dataclass(frozen=True)
class PanelRequest:
    """Multi-agent panel request executed and reconciled by the wrapper."""

    request_id: str
    cycle: int
    kind: str
    members: List[SingleAgentRequest] = field(default_factory=list)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "request_id": self.request_id,
            "cycle": self.cycle,
            "kind": self.kind,
            "members": [member.to_dict() for member in self.members],
        }

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "PanelRequest":
        members_raw = data.get("members", [])
        return cls(
            request_id=str(data.get("request_id", "") or ""),
            cycle=int(data.get("cycle", 0) or 0),
            kind=str(data.get("kind", "") or ""),
            members=[
                SingleAgentRequest.from_dict(item)
                for item in members_raw
                if isinstance(item, dict)
            ],
        )


@dataclass(frozen=True)
class PanelResponse:
    """Normalized panel response, including all member responses."""

    request_id: str
    cycle: int
    kind: str
    overall: str
    summary: str
    member_responses: List[SingleAgentResponse] = field(default_factory=list)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "request_id": self.request_id,
            "cycle": self.cycle,
            "kind": self.kind,
            "overall": self.overall,
            "summary": self.summary,
            "member_responses": [member.to_dict() for member in self.member_responses],
        }

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "PanelResponse":
        members_raw = data.get("member_responses", [])
        return cls(
            request_id=str(data.get("request_id", "") or ""),
            cycle=int(data.get("cycle", 0) or 0),
            kind=str(data.get("kind", "") or ""),
            overall=str(data.get("overall", "") or ""),
            summary=str(data.get("summary", "") or ""),
            member_responses=[
                SingleAgentResponse.from_dict(item)
                for item in members_raw
                if isinstance(item, dict)
            ],
        )


@dataclass(frozen=True)
class PanelExecutionResponse:
    """Raw panel execution result without any reconciliation policy."""

    request_id: str
    cycle: int
    kind: str
    member_responses: List[SingleAgentResponse] = field(default_factory=list)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "request_id": self.request_id,
            "cycle": self.cycle,
            "kind": self.kind,
            "member_responses": [member.to_dict() for member in self.member_responses],
        }

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "PanelExecutionResponse":
        members_raw = data.get("member_responses", [])
        return cls(
            request_id=str(data.get("request_id", "") or ""),
            cycle=int(data.get("cycle", 0) or 0),
            kind=str(data.get("kind", "") or ""),
            member_responses=[
                SingleAgentResponse.from_dict(item)
                for item in members_raw
                if isinstance(item, dict)
            ],
        )
