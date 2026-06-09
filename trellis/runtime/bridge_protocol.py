"""CLI protocol for the trellis Python bridge."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict


@dataclass(frozen=True)
class BridgeCliRequest:
    config_path: Path
    runtime_root: Path
    request: Dict[str, Any]

    def to_dict(self) -> Dict[str, Any]:
        return {
            "config_path": str(self.config_path),
            "runtime_root": str(self.runtime_root),
            "request": dict(self.request),
        }

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "BridgeCliRequest":
        config_raw = str(data.get("config_path", "") or "").strip()
        runtime_raw = str(data.get("runtime_root", "") or "").strip()
        request = data.get("request")
        if not config_raw:
            raise RuntimeError("bridge request is missing config_path")
        if not runtime_raw:
            raise RuntimeError("bridge request is missing runtime_root")
        if not isinstance(request, dict):
            raise RuntimeError("bridge request is missing request object")
        return cls(
            config_path=Path(config_raw).resolve(),
            runtime_root=Path(runtime_raw).resolve(),
            request=request,
        )
