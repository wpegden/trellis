#!/usr/bin/env python3
"""Replay a pinned, optional source witness; never issue proof/approval receipts."""
from __future__ import annotations
import argparse
import base64
import hashlib
import json
from pathlib import Path
import subprocess
import uuid
from scripts import pv_artifact_runner as runner

CATALOG_SCHEMA = "trellis-pv-reproducer-catalog/v1"

def sha(data):
    return hashlib.sha256(data).hexdigest()

def prepare_request(repo: Path, case: dict, toolchain: Path, runtime: Path, *, allow_large=False):
    if case["resource_class"] not in {"small", "large"}:
        raise ValueError("symbolic or historical-only evidence has no executable test")
    if case["resource_class"] == "large" and not allow_large:
        raise ValueError("large-memory reproduction is optional; select --allow-large explicitly")
    relative = "reference/rust-witnesses/target-" + sha(case["target_id"].encode()) + "/witness.rs"
    source = runner._regular_file_beneath(repo, relative, "witness").read_bytes()
    crate = repo / "crate"
    goal = repo / "GOAL.md"
    if sha(source) != case["artifact_sha256"] or runner._tree_digest(crate) != case["pinned_crate_tree_sha256"] or sha(goal.read_bytes()) != case["goal_sha256"]:
        raise ValueError("reproducer source, crate, or GOAL identity changed")
    rustc = toolchain / "bin/rustc"
    cargo = toolchain / "bin/cargo"
    version = subprocess.check_output([rustc, "-vV"], text=True, env={}, timeout=30)
    if version != case["rustc_vv"]:
        raise ValueError("selected compiler differs from catalog")
    memory = int(case["address_space_bytes"])
    if memory <= 0 or (case["resource_class"] == "small" and memory > 2 * 1024**3):
        raise ValueError("invalid resource declaration")
    request = dict(schema=runner.REQUEST_SCHEMA, target_id=case["target_id"],
        relative_path=relative, artifact_source_base64=base64.b64encode(source).decode(),
        artifact_sha256=case["artifact_sha256"], worker_source_path=str(repo / relative),
        pinned_crate_root=str(crate), pinned_crate_tree_sha256=case["pinned_crate_tree_sha256"],
        goal_path=str(goal), goal_sha256=case["goal_sha256"], cargo_lock_sha256=case["cargo_lock_sha256"],
        rustc_path=str(rustc), rustc_vv=version, cargo_path=str(cargo), toolchain_root=str(toolchain),
        dependency_cache_path="", runtime_root=str(runtime), run_id="reproduce-" + uuid.uuid4().hex,
        limits=dict(timeout_seconds=120, cpu_seconds=120, address_space_bytes=memory,
            file_size_bytes=256*1024**2, process_count=2048, stdout_bytes=65536, stderr_bytes=65536))
    if case.get("supplemental_lock"):
        relative_lock = case["supplemental_lock"]
        if not relative_lock.startswith("reference/rust-witnesses/"):
            raise ValueError("supplemental lock must be in the witness evidence directory")
        lock = runner._regular_file_beneath(repo, relative_lock, "supplemental lock").read_bytes()
        if sha(lock) != case["cargo_lock_sha256"]:
            raise ValueError("supplemental lock identity changed")
        request.update(schema=runner.SUPPLEMENTAL_LOCK_REQUEST_SCHEMA,
            supplemental_cargo_lock_base64=base64.b64encode(lock).decode())
    return request

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--case", required=True)
    parser.add_argument("--toolchain-root", required=True, type=Path)
    parser.add_argument("--runtime-root", required=True, type=Path)
    parser.add_argument("--allow-large", action="store_true")
    args = parser.parse_args()
    repo = args.repo.resolve()
    catalog = json.loads(runner._regular_file_beneath(repo,
        "reference/rust-witnesses/catalog.json", "catalog").read_text())
    if catalog.get("schema") != CATALOG_SCHEMA:
        raise ValueError("unsupported catalog schema")
    cases = [case for case in catalog["cases"] if case["id"] == args.case]
    if len(cases) != 1:
        raise ValueError("case must identify exactly one catalog entry")
    args.runtime_root.mkdir(parents=True, exist_ok=True)
    request = prepare_request(repo, cases[0], args.toolchain_root.resolve(),
        args.runtime_root.resolve(), allow_large=args.allow_large)
    prefix = args.runtime_root / request["run_id"]
    prefix.with_suffix(".request.json").write_text(json.dumps(request, indent=2) + "\n")
    try:
        result = runner.run(request)
    except (runner.RunnerUnavailable, runner.SandboxUnavailable, OSError, ValueError) as exc:
        result = runner._result_base(request)
        result["reason"] = str(exc)
    result.update(authority="Corroborating execution only; no Lean closure, bug adjudication, correspondence verdict, or human approval is issued.",
        catalog_sha256=sha((repo / "reference/rust-witnesses/catalog.json").read_bytes()),
        case_id=args.case)
    prefix.with_suffix(".result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"case": args.case, "status": result["status"], "result": str(prefix.with_suffix('.result.json'))}))
    return 0 if result["status"] == "available" else 1

if __name__ == "__main__":
    raise SystemExit(main())
