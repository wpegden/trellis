"""Native preparation diagnostics, not an axiom approval or certificate path."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import shutil

from trellis.json_io import save_json
from trellis.modeling.cache import key
from trellis.modeling.environment import run_source_command
from trellis.pv_execution import execution_timeout

PROBE = Path(__file__).resolve().parents[2] / "scripts/lean_preparation_axioms.lean"
SCHEMA = "trellis-preparation-axioms/v1"
# Same canonical floor as kernel/src/model.rs; regression-tested against it.
CANONICAL_AXIOMS = ("propext", "funext", "Classical.choice", "Quot.sound")


class PreparationAxiomTimeout(ValueError):
    """Incomplete execution evidence; retry before an adaptation handoff."""


def probe_identity() -> str:
    return hashlib.sha256(PROBE.read_bytes()).hexdigest()


def read_report(path: Path, module: str) -> dict:
    report = json.loads(path.read_text())
    if (not isinstance(report, dict) or report.get("schema") != SCHEMA
            or report.get("module") != module or type(report.get("complete")) is not bool
            or not isinstance(report.get("declarations"), list)
            or not isinstance(report.get("error_lines"), list)
            or any(type(line) is not int or line < 1 for line in report["error_lines"])):
        raise ValueError(f"Invalid preparation axiom report: {path}")
    seen = set()
    for row in report["declarations"]:
        if (not isinstance(row, dict) or not isinstance(row.get("name"), str) or not row["name"]
                or row["name"] in seen or type(row.get("pending")) is not bool
                or type(row.get("line")) is not int or row["line"] < 1
                or not isinstance(row.get("axioms"), list)
                or any(not isinstance(name, str) or not name for name in row["axioms"])
                or (row["pending"] and row["axioms"])):
            raise ValueError(f"Invalid declaration in preparation axiom report: {path}")
        seen.add(row["name"])
    complete = not report["error_lines"] and not any(row["pending"] for row in report["declarations"])
    if report["complete"] != complete:
        raise ValueError(f"Inconsistent preparation axiom coverage: {path}")
    return report


def observe_module(path: Path, module: str, *, output: Path, tools: dict, env: dict,
                   environment: str, dependencies: dict, cache, timeout: float | None = None) -> tuple[dict, dict]:
    """Keep partial results, but never cache timeouts or failed probe execution."""
    source_sha = hashlib.sha256(path.read_bytes()).hexdigest()
    identity = key(SCHEMA, probe_identity(), environment, module, source_sha, dependencies)
    report_path, log = output / f"axioms-{module}.json", output / f"axioms-{module}.log"
    cached = cache.get("axioms", identity)
    argv = [tools["lean"], "--run", str(PROBE), str(path), module, str(report_path)]
    budget = execution_timeout(tools, "lean", timeout)
    execution = None
    if cached:
        shutil.copy2(cached / "report.json", report_path)
        shutil.copy2(cached / "log", log)
    else:
        execution = run_source_command(argv, cwd=path.parent, env=env, stdout=log, timeout=budget)
    step = {"name": "lean_axioms", "module": module, "cached": bool(cached), "log": str(log),
            "key": identity, "argv": argv, "exit_code": execution["exit_code"] if execution else 0,
            "seconds": execution["seconds"] if execution else 0,
            "timeout": execution["timeout"] if execution else False, "timeout_seconds": budget}
    if execution:
        step["execution"] = execution
    summary = {"source_sha256": source_sha, "report": str(report_path), "key": identity,
               "timeout": step["timeout"]}
    try:
        if step["exit_code"] != 0 or step["timeout"]:
            raise ValueError(f"Axiom observation failed for {module}; see {log}")
        report = read_report(report_path, module)
        summary.update(report_sha256=hashlib.sha256(report_path.read_bytes()).hexdigest(),
                       complete=report["complete"], declarations=len(report["declarations"]),
                       pending=sum(row["pending"] for row in report["declarations"]))
        if not cached:
            cache.put("axioms", identity, {"report.json": report_path, "log": log})
    except (OSError, ValueError) as error:
        summary["error"] = str(error)
    return summary, step


def compatibility(observation: dict, tcb: dict, *, output: Path) -> dict:
    """Compare observed dependencies to exactly the native bootstrap policy.

    This does not approve discovered assumptions. The ordinary final closure
    gate still checks every owner, including support authored after preparation.
    """
    inventory = observation.get("axiom_dependencies", {})
    if inventory.get("schema") != SCHEMA or inventory.get("probe_sha256") != probe_identity():
        raise ValueError("Missing or stale preparation axiom observation; repeat the full Lean check")
    if inventory.get("environment") != observation.get("environment"):
        raise ValueError("Preparation axiom observation has a different resource environment")
    modules = observation.get("raw_models", {})
    if inventory.get("raw_sha256") != {name: hashlib.sha256(Path(path).read_bytes()).hexdigest()
                                       for name, path in modules.items()}:
        raise ValueError("Extraction changed after its axiom observation; repeat the check")
    namespace = tcb["namespace"]
    approved = set(CANONICAL_AXIOMS) | {
        name if not namespace or name.startswith(namespace + ".") else namespace + "." + name
        for name in tcb["global"]}
    blocked, pending, checked = {}, [], 0
    for name in sorted(modules):
        summary = inventory.get("modules", {}).get(name)
        if summary and summary.get("timeout"):
            raise PreparationAxiomTimeout(
                f"Axiom observation timed out for {name}. Repeat the full Lean check with adequate time "
                "before requesting adaptation audit; a timeout cannot justify a support handoff. "
                + summary.get("error", ""))
        if not summary or summary.get("error"):
            raise ValueError(f"Axiom observation unavailable for {name}: {(summary or {}).get('error', 'not checked')}")
        path = Path(observation["modules"][name])
        if summary["source_sha256"] != hashlib.sha256(path.read_bytes()).hexdigest():
            raise ValueError(f"Elaborated source changed after axiom observation: {name}")
        report_path = Path(summary["report"])
        if summary["report_sha256"] != hashlib.sha256(report_path.read_bytes()).hexdigest():
            raise ValueError(f"Preparation axiom report changed: {name}")
        report = read_report(report_path, name)
        if not report["complete"]:
            pending.append({"module": name, "error_lines": report["error_lines"],
                            "declarations": [row["name"] for row in report["declarations"] if row["pending"]]})
        for row in report["declarations"]:
            if row["pending"]:
                continue
            checked += 1
            for axiom in sorted(set(row["axioms"]) - approved):
                blocked.setdefault(axiom, []).append({"module": name, "declaration": row["name"],
                                                      "line": row["line"], "source": str(path)})
    report = {"schema": SCHEMA, "probe_sha256": probe_identity(), "environment": inventory["environment"],
              "raw_sha256": inventory["raw_sha256"], "approved_axioms": sorted(approved),
              "complete": not pending, "checked_declarations": checked,
              "pending": pending, "unapproved_axioms": blocked}
    save_json(output, report)
    return {"report": str(output), "sha256": hashlib.sha256(output.read_bytes()).hexdigest(),
            "complete": report["complete"], "checked_declarations": checked,
            "pending_declarations": sum(len(item["declarations"]) for item in pending),
            "unapproved_axioms": sorted(blocked)}
