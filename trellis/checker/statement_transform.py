"""Lean-native statement derivation through the existing confined checker.

This produces statements, never certificates or human approvals. Dependency
preparation, replay attestations, transport, locking and atomic sidecar storage
are the same mechanisms used by the other Lean observations.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path

from trellis.atomic_actions import observations
from trellis.checker import telemetry
from trellis.checker.sync import load_local_closure_axioms, store_local_closure_axioms


def _hash_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _failure_output(payload, fallback):
    # Lake puts detailed compiler errors on stdout and often only its final
    # "build failed" summary on stderr. Preserve both for the calling agent.
    parts = [str(payload.get(key) or "").strip()
             for key in ("stdout", "stderr", "spawn_error")]
    if payload.get("timed_out") and not any("timed out" in part.lower() for part in parts):
        parts.append("timed out")
    return "\n".join(dict.fromkeys(part for part in parts if part)) or fallback


def _environment(server, dependencies):
    """Capture current provenance, sharing only this pass's source reads.

    The handler already owns the workspace gate. As in replay readiness, take
    the sync lock inside it, release it before any compilation, and re-read
    captured bytes before admitting the result. No snapshot crosses a build
    or survives this call; the ordered artifact bundles are always rehashed.
    """
    def capture(snapshot):
        pin = server.supervisor_repo / "lean-toolchain"
        if not pin.is_file() or not pin.read_text().strip():
            raise ValueError("statement derivation requires a pinned lean-toolchain")
        order = (snapshot.materialization_order(dependencies) if snapshot is not None
                 else observations.materialization_order(server.supervisor_repo, dependencies))
        deps = {}
        for node in order:
            evidence = observations.read_kernel_replay_artifact_evidence(
                server.supervisor_repo, node, source_snapshot=snapshot)
            if evidence is None:
                raise ValueError(f"statement derivation requires current replay evidence for {node}")
            deps[node] = evidence
        return {
            "toolchain_sha256": observations._kernel_replay_toolchain_sha256(server.supervisor_repo),
            "manifest_sha256": _hash_file(server._lake_manifest_path) if server._lake_manifest_path.exists() else "",
            "dependencies": deps,
        }

    with telemetry.lock(server._sync_lock, "source_snapshot"):
        snapshot = observations.SourceClosureSnapshot(server.supervisor_repo)
        try:
            result = capture(snapshot)
            if not snapshot.is_current():
                raise ValueError("Lean derivation inputs changed during provenance capture; retry")
            return result
        except observations.SourceClosureSnapshotTooLarge:
            # Preserve the existing unbounded-project behavior, without
            # retaining a large snapshot or weakening any evidence check.
            del snapshot
            return capture(None)


def handle(server, request):
    inp = request.raw["input"]
    script_name = "lean_semantic_fingerprint.lean" if "fingerprint_node" in inp else "lean_statement_transform.lean"
    script = Path(__file__).resolve().parents[2] / "scripts" / script_name
    helper_args = ["--statement"] if "fingerprint_node" in inp else []
    dependencies = sorted(observations._kernel_extract_tablet_imports(inp["context"]))

    def failure(error):
        return ({"request_id": request.request_id, "status": "invalid_statement", "error": str(error)}, 1, {})

    try:
        failed = server._ensure_closures_have_current_kernel_replay(dependencies, timeout_secs=request.timeout_secs)
        if failed is not None:
            return failure(_failure_output(failed, "dependency replay failed"))
        provenance = _environment(server, dependencies)
        helper_sha = _hash_file(script)
        key = hashlib.sha256(json.dumps([inp, helper_sha, provenance], sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        cache_dir = server.local_closure_axioms_cache_dir.parent / "statement-transforms"
        cached = load_local_closure_axioms(cache_dir, key, expected_version=1)
        if cached and cached.get("response", {}).get("status") == "ok":
            return ({**cached["response"], "request_id": request.request_id}, 0, {"statement_transform_cache_hit": True})
        payload = observations._run_lake_command(
            server.supervisor_repo, ["env", "lean", "--run", str(script), *helper_args, json.dumps(inp, ensure_ascii=False)],
            timeout_secs=request.timeout_secs, bwrap_role="lake_compiler",
            environment={"ELAN_TOOLCHAIN": (server.supervisor_repo / "lean-toolchain").read_text().strip()},
        )
        if payload.get("timed_out") or payload.get("spawn_error"):
            return failure(_failure_output(payload, "Lean statement derivation failed"))
        # Lean/user elaborators may print diagnostics. Accept one complete
        # structured result only; arbitrary preceding output is diagnostic.
        rows = []
        for line in str(payload.get("stdout", "")).splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if isinstance(row, dict) and row.get("status") in {"ok", "invalid_statement"}:
                rows.append(row)
        if len(rows) != 1:
            return failure(_failure_output(payload, "Lean emitted no unique derivation"))
        row = rows[0]
        if payload.get("returncode") != 0 or row.get("status") != "ok":
            return failure(row.get("error") or "Lean statement derivation failed")
        if _environment(server, dependencies) != provenance or _hash_file(script) != helper_sha:
            return failure("Lean derivation inputs changed during elaboration; retry")
        response = {**row, "helper_sha256": helper_sha, "environment": provenance}
        store_local_closure_axioms(cache_dir, key, node_name="statement", response=response, cache_version=1)
        return ({"request_id": request.request_id, **response}, 0, {"statement_transform_cache_hit": False})
    except (OSError, ValueError) as exc:
        return failure(exc)
