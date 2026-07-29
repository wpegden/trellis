"""Daemon-side pre-validation (E§5) — make kernel rejections rare.

The kernel-side checker-socket validation at apply time is the SOLE
authority; everything here only reduces wasted boundary work. Order:

1. **Snapshot green**: the confirming ``lake build Tablet.<Node>``
   already gated the driver's success (compile_loop.confirm).
2. **Only-BODY audit** (belt over the structural guarantee):
   ``git diff --name-only`` names exactly the node file; ``git status
   --porcelain`` shows no untracked files; re-split of the on-disk
   file yields a byte-identical prefix.
3. **Body scan**: the driver's ban scan re-run on the final body.
4. **Axioms probe**: ``#print axioms <Node>`` via ``lake env lean``;
   reported set ⊆ floor ∪ ``APPROVED_AXIOMS.json`` (global +
   per-node); fail-closed on probe failure.
5. **Local-closure probe**: ``lake env lean --run
   scripts/lean_local_closure.lean <node>`` when the script exists in
   the workspace (the real tablet ships it); reported ``skipped`` when
   absent (micro-workspace tier) — the kernel's gate 8 re-runs it
   authoritatively either way.
6. **Fingerprint stamp** for the spool record.
"""

from __future__ import annotations

import hashlib
import json
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional

from trellis.atomic_actions.observations import tablet_source_closure_hash
from trellis.sidecar.config import SidecarConfig
from trellis.sidecar.driver import (
    body_ban_scan,
    load_approved_axioms_list,
    split_body_marker,
)
from trellis.sidecar.workspace import build_lake_command

PROBE_TIMEOUT_SECS = 300.0


def _sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _sha256_file(path: Path) -> str:
    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        return ""


@dataclass
class PrevalidationResult:
    ok: bool
    reasons: List[str] = field(default_factory=list)
    axioms: List[str] = field(default_factory=list)
    local_closure_status: str = "skipped"
    fingerprints: Dict[str, str] = field(default_factory=dict)

    def daemon_validation(self) -> Dict[str, Any]:
        return {
            "compiled": self.ok or "compile" not in ";".join(self.reasons),
            "axioms_probe": {"ok": self.ok, "axioms": self.axioms},
            "local_closure_probe": {"status": self.local_closure_status},
            "reasons": self.reasons,
        }


def _git_lines(repo: Path, *args: str) -> Optional[List[str]]:
    proc = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True
    )
    if proc.returncode != 0:
        return None
    return [line for line in proc.stdout.splitlines() if line.strip()]


def only_body_audit(repo: Path, node: str, expected_prefix: str) -> List[str]:
    """Independent audit that the workspace diff is EXACTLY the one
    node file's body (catches driver bugs, not just model tricks)."""
    problems: List[str] = []
    rel = f"Tablet/{node}.lean"
    diff = _git_lines(repo, "diff", "--name-only")
    if diff is None:
        problems.append("only_body: git diff failed")
    elif diff != [rel]:
        problems.append(f"only_body: changed files {diff!r} != [{rel!r}]")
    status = _git_lines(repo, "status", "--porcelain")
    if status is None:
        problems.append("only_body: git status failed")
    else:
        untracked = [line for line in status if line.startswith("??")]
        if untracked:
            problems.append(f"only_body: untracked files {untracked!r}")
    try:
        content = (Path(repo) / rel).read_text(encoding="utf-8")
        prefix, _ = split_body_marker(content)
        if prefix != expected_prefix:
            problems.append("only_body: prefix not byte-identical after splice")
    except (OSError, ValueError) as exc:
        problems.append(f"only_body: re-split failed: {exc}")
    return problems


def run_axioms_probe(
    repo: Path, node: str, config: SidecarConfig
) -> tuple[Optional[List[str]], str]:
    """``#print axioms`` via ``lake env lean`` on a 2-line probe file.
    Returns (axioms, log) — axioms None on probe failure (fail-closed)."""
    probe = Path(repo) / ".trellis-sidecar-axioms-probe.lean"
    probe.write_text(f"import Tablet.{node}\n#print axioms {node}\n", encoding="utf-8")
    cmd = build_lake_command(
        repo,
        ["lake", "env", "lean", probe.name],
        lean_threads=config.lean_threads,
        sandbox_role=config.sandbox_role,
        allow_unsandboxed=config.allow_unsandboxed,
    )
    try:
        proc = subprocess.run(
            cmd,
            cwd=str(repo),
            capture_output=True,
            text=True,
            timeout=PROBE_TIMEOUT_SECS,
        )
    except subprocess.TimeoutExpired:
        return None, "axioms probe timeout"
    finally:
        try:
            probe.unlink()
        except OSError:
            pass
    output = proc.stdout + "\n" + proc.stderr
    if proc.returncode != 0:
        return None, output[-2048:]
    # Output shape: `'<Node>' depends on axioms: [a, b, c]` or
    # `'<Node>' does not depend on any axioms`.
    axioms: List[str] = []
    for line in output.splitlines():
        if "depends on axioms:" in line:
            inside = line.split("depends on axioms:", 1)[1].strip()
            inside = inside.strip("[]")
            axioms = [a.strip() for a in inside.split(",") if a.strip()]
            break
        if "does not depend on any axioms" in line:
            axioms = []
            break
    return axioms, output[-2048:]


def run_local_closure_probe(
    repo: Path, node: str, config: SidecarConfig
) -> Dict[str, Any]:
    """Full local-closure probe when the workspace ships the script;
    ``skipped`` otherwise (the kernel gate re-runs it either way)."""
    script = Path(repo) / "scripts" / "lean_local_closure.lean"
    if not script.exists():
        return {"status": "skipped", "reason": "script absent"}
    cmd = build_lake_command(
        repo,
        ["lake", "env", "lean", "--run", str(script), node],
        lean_threads=config.lean_threads,
        sandbox_role=config.sandbox_role,
        allow_unsandboxed=config.allow_unsandboxed,
    )
    try:
        proc = subprocess.run(
            cmd,
            cwd=str(repo),
            capture_output=True,
            text=True,
            timeout=PROBE_TIMEOUT_SECS,
        )
    except subprocess.TimeoutExpired:
        return {"status": "timeout"}
    try:
        payload = json.loads(proc.stdout.strip().splitlines()[-1])
        if isinstance(payload, dict):
            return payload
    except (ValueError, IndexError):
        pass
    return {
        "status": "ok" if proc.returncode == 0 else "error",
        "raw": (proc.stdout + proc.stderr)[-1024:],
    }


def prevalidate_success(
    *,
    repo: Path,
    node: str,
    config: SidecarConfig,
    pre_image: str,
    proof_body: str,
) -> PrevalidationResult:
    result = PrevalidationResult(ok=True)
    prefix, _ = split_body_marker(pre_image)

    # (2) only-BODY audit.
    result.reasons.extend(only_body_audit(repo, node, prefix))

    # (3) final body scan.
    banned = body_ban_scan(proof_body)
    if banned is not None:
        result.reasons.append(f"banned token {banned}")

    # (4) axioms probe, fail-closed.
    axioms, log = run_axioms_probe(repo, node, config)
    if axioms is None:
        result.reasons.append(f"axioms probe failed: {log[-300:]}")
    else:
        result.axioms = axioms
        approved = set(load_approved_axioms_list(repo, node))
        violations = [a for a in axioms if a not in approved]
        if violations:
            result.reasons.append(f"axiom violations {violations!r}")

    # (5) local-closure probe (optional).
    closure = run_local_closure_probe(repo, node, config)
    result.local_closure_status = str(closure.get("status", "skipped"))
    if result.local_closure_status not in ("ok", "skipped"):
        result.reasons.append(
            f"local-closure probe status {result.local_closure_status}"
        )

    # (6) fingerprint stamp.
    final_file = prefix + proof_body
    result.fingerprints = {
        "statement_prefix_sha256": _sha256_text(prefix),
        "source_closure_hash": tablet_source_closure_hash(Path(repo), node) or "",
        "file_sha256_before": _sha256_text(pre_image),
        "file_sha256_after": _sha256_text(final_file),
        "toolchain_sha256": _sha256_file(Path(repo) / "lean-toolchain"),
        "lake_manifest_sha256": _sha256_file(Path(repo) / "lake-manifest.json"),
        "preamble_sha256": _sha256_file(Path(repo) / "Tablet" / "Preamble.lean"),
    }

    result.ok = not result.reasons
    return result
