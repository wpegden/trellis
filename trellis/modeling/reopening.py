"""Resume an audited native extraction rebuild through ordinary preparation.

The previous native repository remains a reference, never a verification seed.
This module does not select source adaptations or import authored tablet nodes.
"""
from __future__ import annotations

import hashlib
import json
import shlex
import subprocess
from pathlib import Path
import shutil
import time

from trellis.modeling.observations import read_json, write_json, fingerprint


def disposition(state: dict) -> dict | None:
    """Recognize the committed terminal outcome, not a pending audit proposal."""
    request = state.get("modeling_request") or {}
    if (state.get("phase") != "Modeling" or state.get("stage") != "Reviewer"
            or state.get("in_flight_request") is not None
            or request.get("action") not in {"reopen", "fail"}):
        return None
    return {"action": request["action"], "reason": request.get("reason", ""),
            "request_id": state.get("request_seq"), "cycle": state.get("cycle")}


def read_disposition(runtime: Path) -> dict | None:
    path = runtime / "protocol_state.json"
    if not path.is_file():
        return None
    # Prefer the run's recorded executable, including when a separate viewer
    # source tree serves it. Status polling must never trigger a Cargo build.
    from trellis.runtime.kernel_cli import KernelCliError, kernel_cli_command
    launch_path = runtime / "launch_env.json"
    launch = read_json(launch_path) if launch_path.is_file() else {}
    configured = (launch.get("env") or {}).get("TRELLIS_TRELLIS_KERNEL_CMD")
    try:
        command = shlex.split(configured) if configured else kernel_cli_command()
    except KernelCliError:
        return disposition(read_json(path))
    if Path(command[0]).name == "cargo":
        return disposition(read_json(path))
    action = "read_modeling_disposition_state"
    read_command = "read-modeling-disposition-state"
    try:
        # The native executable enforces this mode; controller ownership scans
        # can distinguish it from a mutable JSON request while it is running.
        result = subprocess.run([*command, read_command, str(runtime)], input="",
                                capture_output=True, text=True, timeout=30)
        data = json.loads(result.stdout)
        if (result.returncode == 2 and data.get("status") == "invalid_request"
                and data.get("message") == f"unknown runtime command `{read_command}`"):
            result = subprocess.run(command, input=json.dumps({"action": action, "root": str(runtime)}),
                                    capture_output=True, text=True, timeout=30)
            data = json.loads(result.stdout)
    except FileNotFoundError:
        return disposition(read_json(path))
    # A viewer can precede the backend rollout. Only an explicitly unsupported
    # action uses the old reader; corrupt snapshots and other errors propagate.
    if (result.returncode == 2 and data.get("status") == "invalid_request"
            and f"unknown variant `{action}`" in data.get("message", "")):
        return disposition(read_json(path))
    if result.returncode or data.get("status") != "modeling_disposition_state_ok":
        raise RuntimeError(data.get("message") or "Cannot read modeling disposition state")
    return disposition(data["output"])


def reference_paths(root: Path) -> list[str]:
    paths = []
    for path in (root / folder / name for folder in (".trellis/modeling", "modeling-input")
                 for name in ("reopened-from.json", "revision-from.json")):
        if path.is_file():
            paths.extend(read_json(path)["reference_paths"])
    return list(dict.fromkeys(paths))


def capture(preparation: Path, repo: Path, runtime: Path, *, resume: bool = False) -> dict:
    """Validate the native/preparation link and retain the accepted decision."""
    from trellis.modeling.driver import source_fingerprint, check_halt
    from trellis.runtime.bridge import any_halt_marker_present

    check_halt(preparation)
    if any_halt_marker_present(runtime):
        raise ValueError("Resolve the native runtime halt before reopening preparation")
    if (runtime / "pause_request.json").exists() and not resume:
        raise ValueError("An operator pause takes precedence over automatic reopening")
    raw = (runtime / "protocol_state.json").read_bytes()
    import json
    state = json.loads(raw)
    end = disposition(state)
    if not end or end["action"] != "reopen":
        raise ValueError("Native runtime has no committed audit-approved preparation reopening")
    meta = read_json(runtime / "runtime_metadata.json")
    if Path(meta["repo_path"]).resolve() != repo.resolve():
        raise ValueError("Native runtime belongs to a different repository")
    manifest = read_json(repo / "modeling-input/manifest.json")
    if (manifest.get("provenance_schema") != "trellis-native-preparation/v1"
            or Path(manifest["preparation_root"]).resolve() != preparation.resolve()):
        raise ValueError("Native repository belongs to a different preparation")
    old = read_json(preparation / ".trellis/modeling/state.json")
    if old.get("status") != "prepared" or old.get("native_repo") != str(repo):
        raise ValueError("Preparation did not hand off this native repository")
    if source_fingerprint(preparation) != manifest["source_basis"]:
        raise ValueError("The accepted preparation basis has changed")
    if fingerprint(repo / "crate") != fingerprint(preparation / "work/crate"):
        raise ValueError("The frozen native source differs from preparation")
    if "paper_input" in read_json(preparation / "config.json"):
        raise ValueError("Paper implementation v1 freezes its exported generation; initialize a fresh paper preparation instead of legacy GOAL reopening")
    if (repo / "GOAL.md").read_bytes() != (preparation / "GOAL.md").read_bytes():
        raise ValueError("Native GOAL meanings differ from preparation")
    config = read_json(preparation / "config.json")
    declined = {**old.get("declined", {}), **state.get("modeling_declines", {})}
    goals = {g["id"] for g in config["goals"]}
    if not set(declined) <= goals or set(state["configured_targets"]) != goals - set(declined):
        raise ValueError("Pursued goals do not match the audited decline history")
    routes = list(old.get("route_decisions", []))
    for decision in state.get("modeling_route_decisions", []):
        routes.append({"sequence": decision["request_id"], "stage": "native_modeling",
                       "resolution": decision["resolution"], "assessment": decision["assessment"],
                       "native_record": decision})
    return {"schema": "trellis-native-reopening/v1", "disposition": end,
            "preparation": str(preparation), "repo": str(repo), "runtime": str(runtime),
            "protocol_state_sha256": hashlib.sha256(raw).hexdigest(),
            "source_basis": manifest["source_basis"], "declined": declined,
            "audit_assessment": state.get("reviewer_comments", ""),
            "route_decisions": routes,
            "reference_paths": list(dict.fromkeys([*reference_paths(preparation),
                                                    str(preparation), str(repo), str(runtime)]))}


def initialize_reopened(root: Path, record: dict) -> None:
    """Retain the candidate and decisions, resetting both preparation approvals.

    Only called before publishing the new preparation. If initialization was
    interrupted, its partial output is preserved before retrying. Once the
    receipt exists, this function never overwrites agent work.
    """
    from trellis.modeling.driver import initialize, Driver, runtime_root
    from scripts.extract_pv_model import copy_source_inputs

    receipt = root / ".trellis/modeling/reopened-from.json"
    if receipt.exists():
        if read_json(receipt) != record:
            raise ValueError("Existing reopened preparation has a different predecessor")
        return
    if root.exists():
        partial = root.with_name(root.name + f"-interrupted-init-{time.time_ns()}")
        root.rename(partial)
        old_runtime = runtime_root(root)
        if old_runtime.exists():
            old_runtime.rename(runtime_root(partial))
    previous = Path(record["preparation"])
    inputs = read_json(previous / "config.json")
    root.parent.mkdir(parents=True, exist_ok=True)
    initialize(root, previous / "upstream", previous / "goals.json", inputs["tools"],
               model=inputs["model"], effort=inputs["effort"],
               config_template=previous / "trellis.config.json")
    # Retain the operator's launch template, not the preparation's derived config.
    config = read_json(root / "config.json")
    if "launch_config" in inputs:
        config["launch_config"] = inputs["launch_config"]
    else:
        config.pop("launch_config", None)
    write_json(root / "config.json", config)
    shutil.rmtree(root / "work")  # Newly initialized, unpublished scratch only.
    copy_source_inputs(previous / "work", root / "work")
    driver = Driver(root)
    driver.state.update(declined=record["declined"], route_decisions=record["route_decisions"],
                        feedback="The native audit approved reopening preparation. Read the predecessor record and implement the accepted repair through the ordinary stage criteria.",
                        next={"role": "reviewer"})
    # This is the same setup re-entry used by preparation audit/route approval.
    driver.reopen_setup()
    for decision in reversed(record["route_decisions"]):
        resolution = decision["resolution"]
        if resolution.get("action") == "approve":
            driver.state.update(scope=resolution["scope"], limitations=resolution.get("limitations", ""))
            break
    driver.save()
    write_json(receipt, record)  # Publication barrier: all initial state is ready.
