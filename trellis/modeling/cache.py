"""Successful stage artifacts, separated by supervisor and worker ownership."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import shutil
import tempfile


def key(*values: object) -> str:
    return hashlib.sha256(json.dumps(values, sort_keys=True).encode()).hexdigest()


class Cache:
    def __init__(self, writable: Path, readable: list[Path] = ()):
        self.writable = writable
        self.readable = [*readable, writable]

    def get(self, stage: str, identity: str) -> Path | None:
        for root in self.readable:
            entry = root / stage / identity
            if (entry / "complete").is_file():
                return entry
        return None

    def put(self, stage: str, identity: str, artifacts: dict[str, Path]) -> None:
        parent = self.writable / stage
        parent.mkdir(parents=True, exist_ok=True)
        target = parent / identity
        if target.exists():
            return
        with tempfile.TemporaryDirectory(dir=parent, prefix="pending-") as temporary:
            pending = Path(temporary) / "entry"
            pending.mkdir()
            for name, source in artifacts.items():
                if source.is_dir():
                    shutil.copytree(source, pending / name)
                else:
                    shutil.copy2(source, pending / name)
            (pending / "complete").touch()
            try:
                pending.rename(target)
            except OSError:
                if not (target / "complete").is_file():
                    raise
