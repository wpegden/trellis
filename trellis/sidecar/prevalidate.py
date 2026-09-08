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
4. **Axioms probe**: ``#print axioms _root_.<Node>`` via ``lake env lean``;
   reported set ⊆ floor ∪ ``APPROVED_AXIOMS.json`` (global +
   per-node); fail-closed on probe failure.
5. **Local-closure probe**: ``lake env lean --run
   scripts/lean_local_closure.lean <node> --principal=<node>`` when the script exists in
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


def _trellis_scripts_dir() -> Path:
    """`<trellis source>/scripts` — the same resolution the checker server
    and `sandbox._trellis_source_scripts_dir()` use."""
    return Path(__file__).resolve().parents[2] / "scripts"


def _safe_reason(text: Any, *, limit: int = 300) -> str:
    """Neutralize agent-influenced substrings interpolated into a
    ``reasons`` entry: filenames the grunt can name, compiler-log tails
    that quote the proof body verbatim. `daemon_validation.reasons` is
    inert for the kernel today, but it is persisted and reachable, so it
    must not carry raw newlines/control characters or unbounded prose.
    Drop control characters, collapse whitespace runs, cap the length."""
    cleaned = "".join(
        " " if (ord(ch) < 0x20 or 0x7F <= ord(ch) < 0xA0) else ch
        for ch in str(text)
    )
    return " ".join(cleaned.split())[:limit]


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
        problems.append(
            f"only_body: changed files {_safe_reason(repr(diff))} != [{rel!r}]"
        )
    status = _git_lines(repo, "status", "--porcelain")
    if status is None:
        problems.append("only_body: git status failed")
    else:
        untracked = [line for line in status if line.startswith("??")]
        if untracked:
            problems.append(
                f"only_body: untracked files {_safe_reason(repr(untracked))}"
            )
    try:
        content = (Path(repo) / rel).read_text(encoding="utf-8")
        prefix, _ = split_body_marker(content)
        if prefix != expected_prefix:
            problems.append("only_body: prefix not byte-identical after splice")
    except (OSError, ValueError) as exc:
        problems.append(f"only_body: re-split failed: {_safe_reason(exc)}")
    return problems


def run_axioms_probe(
    repo: Path, node: str, config: SidecarConfig
) -> tuple[Optional[List[str]], str]:
    """Root-qualified ``#print axioms`` via ``lake env lean`` on a 2-line probe file.
    Returns (axioms, log) — axioms None on probe failure (fail-closed)."""
    probe = Path(repo) / ".trellis-sidecar-axioms-probe.lean"
    probe.write_text(
        f"import Tablet.{node}\n#print axioms _root_.{node}\n",
        encoding="utf-8",
    )
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
    #
    # Mirrors the kernel's `parse_print_axioms_output`
    # (`kernel/src/runtime_cli_observations.rs`) deliberately, because the
    # two must agree about what an axiom set IS. Two ways the old
    # line-scoped parse failed OPEN, both silent:
    #
    #   * Lean's message formatter WRAPS a long axiom list, so
    #     `[propext,\n Classical.choice, Quot.sound, sorryAx]` parsed as
    #     just `[propext]` — a subset of the approved set, so it passed,
    #     with `sorryAx` dropped on the floor. Normalizing whitespace over
    #     the whole output first is exactly why the kernel does that.
    #   * Output carrying NEITHER marker returned an empty list as a
    #     successful probe, i.e. "no axioms" — indistinguishable from a
    #     clean result. The kernel returns None there. So does this now,
    #     and None is the caller's fail-closed signal.
    normalized = " ".join(output.split())
    # Forgery gate, mirroring the kernel's `parse_print_axioms_output`: the
    # import runs any `initialize` block the module carries, so an untrusted
    # body can print a fake verdict BEFORE Lean's real one. A clean run emits
    # exactly one verdict; two means one of them is injected, and we cannot
    # tell which, so fail closed.
    clean_marker = "does not depend on any axioms"
    deps_marker = "depends on axioms:"
    if normalized.count(clean_marker) + normalized.count(deps_marker) > 1:
        return None, output[-2048:]
    if clean_marker in normalized:
        return [], output[-2048:]
    marker = deps_marker
    start = normalized.find(marker)
    if start < 0:
        return None, output[-2048:]
    after = normalized[start + len(marker):].strip()
    if not after.startswith("["):
        return None, output[-2048:]
    body = after[1:].split("]", 1)[0].strip()
    axioms: List[str] = [a.strip() for a in body.split(",") if a.strip()]
    return axioms, output[-2048:]


def run_local_closure_probe(
    repo: Path, node: str, config: SidecarConfig
) -> Dict[str, Any]:
    """Full local-closure probe; ``skipped`` when the script is absent
    (the kernel gate re-runs it authoritatively either way).

    The script is resolved from the TRELLIS SOURCE tree, not from the
    tablet repo. It has never shipped inside a tablet — `<repo>/scripts/`
    does not exist on the live run, nor in any grunt workspace — so
    looking for it there made this probe permanently `skipped` and left
    the `#print axioms` fallback as the only axiom check that ever ran.
    `trellis/checker/server.py` resolves it the same way, and
    `sandbox._trellis_source_scripts_dir()` ro-binds this directory into
    the `lake_compiler` role, which is the role these probes run under.
    """
    script = _trellis_scripts_dir() / "lean_local_closure.lean"
    if not script.exists():
        return {"status": "skipped", "reason": "script absent"}
    cmd = build_lake_command(
        repo,
        [
            "lake",
            "env",
            "lean",
            "--run",
            str(script),
            node,
            f"--principal={node}",
        ],
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

    # (4) LOCAL-closure probe, and (5) the axiom check DERIVED FROM IT.
    #
    # The axiom set that decides an attempt is the LOCAL one. The kernel's
    # gate 8 runs `run_local_closure_axioms` — the Lean script's
    # `ProofMayAssumeTheorems` mode, which stops on reaching a Tablet
    # theorem and walks only its TYPE — so an open dependency's `sorryAx`
    # is structurally ABSENT from `kernel_axioms`. Closing a node whose
    # dependencies are still open is a thing a grunt is explicitly
    # permitted to do, exactly as a worker may.
    #
    # This previously probed `#print axioms <Node>`, the TRANSITIVE walk,
    # and rejected on its result. Every node importing an open node
    # therefore reported `sorryAx`, failed here, and was discarded BEFORE
    # publishing — so the kernel's gate, which would have accepted it,
    # never saw it. Live-run node `linegraph2_5` (2026-07-31): body
    # compiled clean through `body_ban_scan`, `check_body` and `confirm`,
    # four open imports, discarded after 283s and 1.37M tokens. Five
    # attempts died that way before it was noticed.
    #
    # The invariant this restores, and the one the module docstring
    # already implies: prevalidate is ADVISORY, so a check STRICTER than
    # the kernel's is a bug by construction, never a safety margin. It may
    # only reject what the kernel would also reject.
    closure = run_local_closure_probe(repo, node, config)
    result.local_closure_status = str(closure.get("status", "skipped"))
    if result.local_closure_status not in ("ok", "skipped"):
        result.reasons.append(
            f"local-closure probe status {result.local_closure_status}"
        )
    approved = set(load_approved_axioms_list(repo, node))
    if result.local_closure_status == "ok":
        # Same source the kernel installs its record from.
        result.axioms = [str(a) for a in (closure.get("kernel_axioms") or [])]
        violations = [a for a in result.axioms if a not in approved]
        if violations:
            result.reasons.append(f"axiom violations {violations!r}")
    else:
        # Micro-workspace tier: the script is absent, so fall back to
        # `#print axioms`. `sorryAx` is dropped from the violation set
        # because the transitive walk is precisely the notion the local
        # one exists to replace — a `sorry` in the body itself is already
        # caught by `body_ban_scan` and by `check_body`'s sorry arm, and
        # the kernel's gate 8 re-runs the local probe authoritatively.
        axioms, log = run_axioms_probe(repo, node, config)
        if axioms is None:
            result.reasons.append(f"axioms probe failed: {_safe_reason(log[-300:])}")
        else:
            result.axioms = axioms
            violations = [
                a for a in axioms if a not in approved and a != "sorryAx"
            ]
            if violations:
                result.reasons.append(f"axiom violations {violations!r}")

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
