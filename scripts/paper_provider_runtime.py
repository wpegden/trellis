"""Retain and enforce provider tooling identity for paper launches."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
BINDING_ENV = "TRELLIS_LAUNCH_PROVIDER_SNAPSHOT"


def query(job_dir: Path, context: dict, environment: dict, evidence: Path) -> dict:
    command = [sys.executable, "-m", "trellis.agents.model_preflight", "--config", str(job_dir / "config-template.json"),
               "--work-dir", context["work_dir"], "--burst-home", context["burst_home"]]
    if context.get("engineering_home"):
        command.extend(["--engineering-work-dir", context["engineering_work"], "--engineering-burst-home", context["engineering_home"]])
        environment = {**environment, "TRELLIS_ENGINEERING_CARGO": context["engineering_cargo"]}
    from trellis.pv_execution import owned_process
    evidence.mkdir(parents=True, exist_ok=True)
    with owned_process(command, cancel_grace=0.5, cwd=ROOT, env=environment,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as process:
        stdout, stderr = process.communicate(timeout=180)
        result = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
    (evidence / "stdout.json").write_text(result.stdout)
    (evidence / "stderr.txt").write_text(result.stderr)
    if result.returncode:
        raise ValueError(f"Provider model/effort preflight failed: {result.stderr.strip()[-2000:]}; evidence: {evidence}")
    return json.loads(result.stdout)


def capture(job_dir: Path, environment: dict) -> dict:
    from trellis.burst_home import seed_burst_home
    probe = job_dir / "provider-preflight"
    work = probe / "work"
    work.mkdir(parents=True)
    subprocess.run(["git", "init", "--quiet", str(work)], check=True, env=environment, timeout=15)
    home = seed_burst_home(probe, "catalog", provider="codex", source_home=Path(environment["HOME"]))
    context = {"launch_path": environment.get("PATH"), "work_dir": str(work), "burst_home": str(home)}
    if (job_dir / "engineering-inputs/engineering_contract.md").is_file():
        from trellis.agents.engineering_context import seed
        engineering_work = probe / "engineering-work"
        engineering_work.mkdir()
        context.update(engineering_work=str(engineering_work),
                       engineering_home=str(seed(probe, source_home=Path(environment["HOME"]), work_dir=engineering_work)),
                       engineering_cargo=json.loads((job_dir / "tools.json").read_text())["cargo"])
    return {**context, "receipt": query(job_dir, context, environment, probe)}


def refresh(job_dir: Path, snapshot: dict) -> None:
    """Recheck advertised availability at admission without changing the snapshot."""
    from scripts import paper_launch_snapshot as snapshots
    pinned = snapshot.get("provider_preflight")
    if pinned is None:
        raise ValueError("Provider preflight is required before paper preparation; create a fresh job")
    evidence = job_dir / "provider-observations" / uuid.uuid4().hex
    receipt = query(job_dir, pinned, snapshots.replay_environment(snapshot), evidence)
    validate({**pinned, "receipt": receipt}, snapshot["environment"])
    original = pinned["receipt"]
    for kind, expected in original["runtimes"].items():
        if any(receipt["runtimes"][kind][key] != expected[key] for key in
               ("resolved_executable", "executable_sha256", "cli_version", "effective_path")):
            raise ValueError("Provider runtime changed since the immutable preflight")
    fields = ("role", "model", "effort", "configuration_sha256", "context_sha256")
    if [[row[key] for key in fields] for row in receipt["bindings"]] != [[row[key] for key in fields] for row in original["bindings"]]:
        raise ValueError("Provider selection or configuration changed since the immutable preflight")
    with (evidence / "admission.json").open("x") as stream:
        json.dump({"snapshot": snapshots.snapshot_binding(job_dir), "observed_at_epoch": time.time(), "receipt": receipt}, stream, indent=2)


def validate(record: dict, environment: dict) -> None:
    from trellis.agents.model_preflight import SCHEMA, native_executable_sha256
    receipt = record.get("receipt", {})
    if receipt.get("schema") != SCHEMA or record.get("launch_path") != environment.get("PATH"):
        raise ValueError("Provider preflight identity or frozen PATH changed")
    for identity in receipt.get("runtimes", {}).values():
        executable = shutil.which("codex", path=identity["effective_path"])
        if executable != identity["executable"] or str(Path(executable).resolve()) != identity["resolved_executable"]:
            raise ValueError("Provider executable resolution changed since launch preflight")
        if native_executable_sha256(Path(identity["resolved_executable"])) != identity["executable_sha256"]:
            raise ValueError("Provider executable bytes changed since launch preflight")


def admit_burst(config, work_dir: Path, burst_home: Path, *, execution_kind="headless") -> dict | None:
    """Bind actual role execution to the immutable launch catalog and binary."""
    path = os.environ.get(BINDING_ENV)
    if not path:
        return None
    from scripts import paper_launch_snapshot as snapshots
    from trellis.agents.model_preflight import catalog_arguments, validate_preflight_identity, validate_preflight_configuration, _digest
    job_dir = Path(path).parent.parent
    if Path(path) != job_dir / snapshots.SNAPSHOT:
        raise ValueError("Invalid provider launch snapshot path")
    snapshot = snapshots.validate_snapshot(job_dir)
    record = snapshot.get("provider_preflight")
    if record is None:
        raise ValueError("Paper launch has no provider preflight evidence")
    receipt = record["receipt"]
    binding = dict(role="actual role", model=config.model, effort=config.effort,
                   extra_args=list(config.extra_args or []))
    roots, overrides = catalog_arguments(binding)
    configuration = _digest([execution_kind, roots, overrides])
    admitted = any(row["execution_kind"] == execution_kind and row["configuration_sha256"] == configuration
                   and config.model in row.get("admitted_models", [row["model"]])
                   and row["effort"] == config.effort for row in receipt["bindings"])
    if not admitted:
        raise ValueError("Actual provider model/effort/configuration was not admitted at launch")
    context = validate_preflight_configuration(receipt, binding, work_dir=work_dir, burst_home=burst_home, execution_kind=execution_kind)
    identity = validate_preflight_identity(receipt, work_dir=work_dir, burst_home=burst_home, execution_kind=execution_kind)
    return {"snapshot": snapshots.snapshot_binding(job_dir), "job_dir": str(job_dir),
            "model": config.model, "effort": config.effort, "configuration_sha256": configuration,
            "runtime": identity, "configuration_context": context, "cli_version": receipt["runtimes"][execution_kind]["cli_version"]}
