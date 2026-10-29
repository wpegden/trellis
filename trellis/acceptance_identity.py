"""Acceptance build/deployment provenance shared through one source manifest.

The compiler and reviewer enumerate the same bounded source trees. Each leaf
hash binds a relative path and its bytes; the ordered leaves bind the complete
source set, including additions and removals. This deliberately includes more
than a guessed call graph of authoritative checks.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess

MANIFEST = "acceptance-sources.json"


def source_manifest(root: Path) -> dict:
    spec = json.loads((root / MANIFEST).read_text())
    if spec["schema"] != "trellis-worker-acceptance-v2":
        raise ValueError("Unsupported acceptance source manifest")
    names = set(spec["files"])
    def ignored(name):
        return (name in spec["exclude_directories"]
                or any(name.startswith(p) for p in spec["exclude_prefixes"])
                or any(name.endswith(p) for p in spec["exclude_suffixes"]))
    for tree in spec["trees"]:
        if not (root / tree).is_dir():
            raise FileNotFoundError(root / tree)
        for directory, dirs, files in os.walk(root / tree):
            dirs[:] = sorted(d for d in dirs if not ignored(d))
            for name in files:
                path = Path(directory) / name
                if not ignored(name) and path.suffix.lstrip(".") in spec["extensions"]:
                    names.add(path.relative_to(root).as_posix())
    sources = [{"path": name, "sha256": hashlib.sha256((root / name).read_bytes()).hexdigest()}
               for name in sorted(names)]
    digest = hashlib.sha256((spec["schema"] + "\0").encode())
    for entry in sources:
        digest.update((entry["path"] + "\0" + entry["sha256"] + "\0").encode())
    return {"schema": spec["schema"], "identity": spec["schema"] + ":sha256:" + digest.hexdigest(),
            "sources": sources}


def source_identity(root: Path) -> str:
    try:
        return source_manifest(root)["identity"]
    except (OSError, ValueError, KeyError) as error:
        return f"unavailable:{type(error).__name__}"


def record_deployment(source: Path, binary: Path, receipt: Path) -> dict:
    """Bind the installed checker tree and binary to its embedded build inputs.

    The kernel must attest to the actually installed tree before work starts.
    Query failures retain evidence and fail closed, including old binaries.
    """
    record = {"source_identity": source_identity(source),
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "verified": False}
    try:
        result = subprocess.run([str(binary)], input='{"action":"acceptance_implementation_identity"}\n',
                                text=True, capture_output=True, timeout=15)
        build = json.loads(result.stdout)
        if (result.returncode == 0 and isinstance(build, dict)
                and isinstance(build.get("identity"), str)
                and build["identity"].startswith("trellis-worker-acceptance-v2:")):
            record["build"] = build
            record["verified"] = build["identity"] == record["source_identity"]
        else:
            record["unavailable"] = "Kernel does not supply a v2 build identity"
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        record["unavailable"] = type(error).__name__
    receipt.write_text(json.dumps(record, indent=2) + "\n")
    if not record["verified"]:
        raise RuntimeError(f"Acceptance implementation mismatch between kernel build and deployed checker source; see {receipt}")
    return record
