"""Managed PV revision handoff and successor preparation.

The completed native runtime is immutable. Human acceptance creates a new
preparation, with the ordinary modeling gate and no inherited verdicts.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import shutil
import time

from trellis.json_io import save_json
from trellis.modeling.observations import read_json


def read_handoff(runtime: Path) -> dict | None:
    marker = runtime / "pv_revision_ready.json"
    if not marker.is_file():
        return None
    value = read_json(marker)
    return value.get("pv_revision_handoff", value.get("handoff", value))


def revision_root(runtime: Path) -> Path:
    return runtime / "pv-revision"


def status(runtime: Path) -> dict | None:
    if not read_handoff(runtime):
        return None
    root = revision_root(runtime)
    path = root / "state.json"
    state = read_json(path) if path.is_file() else {}
    return {"controller": "pv_revision", "state": state.get("status", "ready"),
            "phase": state.get("phase", "audit"), "root": str(root),
            "sequence": state.get("sequence", 0), "feedback": state.get("feedback", ""),
            "role": state.get("next", {}).get("role"),
            "digest": state.get("human_package", {}).get("digest"),
            "successor": state.get("successor"), "human_decisions": state.get("human_decisions", [])[-1:]}


def capture(runtime: Path) -> dict:
    """Check the authoritative handoff and freeze its exact version identities."""
    from trellis.runtime.bridge import any_halt_marker_present
    if any_halt_marker_present(runtime):
        raise ValueError("Resolve the native halt before starting its PV revision")
    state_bytes = (runtime / "protocol_state.json").read_bytes()
    state = json.loads(state_bytes)
    handoff = state.get("pv_revision_handoff")
    if not handoff or handoff.get("schema_version") != 1 or state.get("in_flight_request"):
        raise ValueError("No committed PV revision handoff")
    if state.get("phase") != "ProofFormalization" or handoff["cycle"] != state["cycle"]:
        raise ValueError("The completed formalization and revision handoff disagree")
    repo = Path(read_json(runtime / "runtime_metadata.json")["repo_path"]).resolve()
    tablet_manifest = checked_tablet_manifest(repo, state)
    manifest = read_json(repo / "modeling-input/manifest.json")
    if manifest.get("provenance_schema") != "trellis-native-preparation/v1":
        raise ValueError("PV revisions require a native preparation provenance record")
    preparation = Path(manifest["preparation_root"]).resolve()
    from trellis.modeling.native import check_provenance
    verified = check_provenance(repo, output=runtime / "pv-revision-preflight" / str(time.time_ns()),
                                bwrap_role="lake_compiler")
    if not verified["passed"]:
        raise ValueError("The predecessor's source/model provenance is no longer current")
    collection = read_json(preparation / "goals.json")
    known = {goal["id"] for goal in collection["goals"]}
    negative = []
    for item in handoff["refuted_goals"]:
        primary = item["goal_id"]
        goal = state.get("authored_decide_goals", {}).get(primary, primary)
        if goal not in known:
            raise ValueError(f"Refuted target {primary} has no supplied GOAL identity")
        negative.append({**item, "goal_id": goal, "challenge_id": primary})
    inputs = read_json(preparation / "config.json")
    return {"schema": "trellis-pv-revision-predecessor/v1", "repo": str(repo),
            "runtime": str(runtime), "preparation": str(preparation),
            "protocol_state_sha256": hashlib.sha256(state_bytes).hexdigest(),
            "tablet_manifest": tablet_manifest,
            "handoff": handoff, "negated_goals": negative, "collection": collection,
            "source": str(preparation / "upstream"), "tools": inputs["tools"],
            "reference_paths": [str(repo), str(runtime), str(preparation)],
            "modeling_gate_required": True,
            "baseline": {"nodes": sorted(state.get("live", {}).get("present_nodes", [])),
                         "dependencies": state.get("deps", {}),
                         "node_role": state.get("node_role", {})}}


def checked_tablet_manifest(repo: Path, state: dict) -> dict:
    """Match the preserved bytes to the kernel's completed closure records.

    The kernel's active_decl_hash is plain SHA256 of the complete raw Lean
    file, not the declaration pretty-print or the tagged source-tree hash.
    """
    from trellis.pv_revision.store import source_manifest
    manifest = source_manifest(repo / "Tablet")
    files = {row["relative_path"]: row["sha256_of_raw_bytes"] for row in manifest["files"]}
    records = state.get("local_closure_records", {})
    refutations = {node for goal in state["pv_revision_handoff"]["refuted_goals"]
                   for node in goal.get("refutation_nodes", [])}
    if not refutations or not refutations <= records.keys():
        raise ValueError("Completed refutations lack closure source identities")
    present = set(state.get("live", {}).get("present_nodes", []))
    for node, record in records.items():
        expected = record.get("active_decl_hash")
        if node in refutations or (node in present and expected):
            if not expected or files.get(node + ".lean") != expected:
                raise ValueError(f"Completed proof source differs from its closure record: {node}")
    return manifest


def initialize_runtime(runtime: Path) -> Path:
    """Idempotent publication; partial initialization is preserved for diagnosis."""
    from trellis.pv_revision.driver import initialize
    root = revision_root(runtime)
    if (root / "state.json").is_file():
        saved = read_json(root / "state.json")["handoff"]
        raw = (runtime / "protocol_state.json").read_bytes()
        if saved["protocol_state_sha256"] != hashlib.sha256(raw).hexdigest():
            raise ValueError("The revision predecessor changed after initialization")
        preserve_predecessor(root, saved)
        return root
    record = capture(runtime)
    if root.exists():
        root.rename(root.with_name(root.name + f"-interrupted-{time.time_ns()}"))
    metadata = read_json(runtime / "runtime_metadata.json")
    template = Path(metadata.get("config_path") or Path(record["preparation"]) / "trellis.config.json")
    initialize(root, Path(record["source"]), record["collection"], record["negated_goals"],
               config_template=template, handoff=record, tools=record["tools"])
    preserve_predecessor(root, record)
    return root


def preserve_predecessor(root: Path, record: dict) -> None:
    """Preserve proof sources once; never copy compiled caches or whole logs."""
    from trellis.pv_revision.store import copy_source_tree, source_manifest
    evidence = root / "predecessor"
    evidence.mkdir(exist_ok=True)
    state_file = evidence / "protocol_state.json"
    if not state_file.exists():
        shutil.copy2(Path(record["runtime"]) / "protocol_state.json", state_file)
    if hashlib.sha256(state_file.read_bytes()).hexdigest() != record["protocol_state_sha256"]:
        raise ValueError("Preserved protocol evidence differs from the completed generation")
    from trellis.dispatch.history import copy_state_objects
    copy_state_objects(read_json(state_file), Path(record["repo"]), evidence)
    manifest = evidence / "tablet-manifest.json"
    if not manifest.exists():
        target = evidence / "Tablet"
        if target.exists():
            target.rename(evidence / f"Tablet-interrupted-{time.time_ns()}")
        copy_source_tree(Path(record["repo"]) / "Tablet", target)
        save_json(manifest, source_manifest(target))
    elif source_manifest(evidence / "Tablet") != read_json(manifest):
        raise ValueError("Preserved proof sources changed")
    if read_json(manifest) != record["tablet_manifest"]:
        raise ValueError("Preserved proof sources differ from the captured formalization")
    save_json(evidence / "record.json", record)
    (evidence / "README.md").write_text(
        "# Previous formalization\n\n"
        "These are the statements, proof bodies, dependencies and evidence of the completed predecessor. "
        "Reuse them as candidates through ordinary modeling and worker acceptance. "
        "A previous closure or review is evidence about its recorded source and meanings; "
        "it is not an approval of this revision. Recheck applicability to the new source, "
        "definitions, assumptions and requirement inventory.\n")


def approved_collection(state: dict) -> dict:
    collection = state.get("proposed_collection", {**state["original_collection"], "goals": state["proposed_goals"]})
    if collection["goals"] != state["proposed_goals"]:
        raise ValueError("The approved goal inventory and collection disagree")
    return collection


def successor_config(root: Path, template: Path) -> Path:
    """Carry the revision's current launch policy and opt-in into its successor."""
    current = root / "trellis.config.json"
    config = read_json(current if current.is_file() else template)
    config.setdefault("pv_tablet", {}).setdefault("revision", {})["enabled"] = True
    path = root / "successor-config.json"
    if path.exists() and read_json(path) != config:
        raise ValueError("Successor launch settings changed")
    if not path.exists():
        save_json(path, config)
    return path


def prepare_successor(root: Path, destination: Path, *, config_template: Path) -> dict:
    """Prepare the next generation without carrying any approval over by name."""
    from trellis.pv_revision.driver import Driver
    from trellis.pv_revision.store import source_manifest, ratified_source_manifest, ratified_source, REVIEW_SCHEMA
    from trellis.modeling.driver import initialize
    driver = Driver(root)
    driver._current()
    state = driver.state
    if state["status"] not in {"accepted_for_modeling", "successor_preparing", "successor_started"}:
        raise ValueError("The source and requirement revision has not been ratified")
    package = driver._package(state)
    decisions = state["human_decisions"]
    if not decisions or decisions[-1]["action"] != "accept" or decisions[-1]["digest"] != package["digest"]:
        raise ValueError("The accepted revision package changed before successor preparation")
    if package['schema'] == REVIEW_SCHEMA and (
        decisions[-1].get('ratified_candidate_digest') != package['candidate_digest']
        or decisions[-1].get('attached_evidence_digest') != package['evidence_digest']
        or decisions[-1].get('evidence_ratified') is not False
    ):
        raise ValueError('The production ratification identity is missing or changed')
    source = ratified_source(root, package)
    manifest = ratified_source_manifest(package)
    goals = approved_collection(state)
    if not goals["goals"]:
        raise ValueError("All requirements were retired; there is no successor to verify")
    config_template = successor_config(root, config_template)
    receipt = {"schema": "trellis-pv-revision-successor/v1", "revision_root": str(root),
               "package_digest": package["digest"], "predecessor": state["handoff"],
               "reference_paths": [*state["handoff"]["reference_paths"], str(root)],
               "revalidation": "Reuse prior proofs where applicable. The successor preserves verification only where its inputs still match; use current runtime status for outstanding checks."}
    marker = destination / ".trellis/modeling/revision-from.json"
    if marker.is_file():
        if read_json(marker) != receipt or read_json(destination / "goals.json") != goals:
            raise ValueError("Successor preparation belongs to a different revision")
        if source_manifest(destination / "upstream") != manifest:
            raise ValueError("Successor source differs from the ratified candidate")
        return receipt
    if destination.exists():
        destination.rename(destination.with_name(destination.name + f"-interrupted-{time.time_ns()}"))
    goal_file = root / "approved-goals.json"
    save_json(goal_file, goals)
    config = read_json(config_template)
    provider = config["worker"]
    initialize(destination, source, goal_file, state["tools"],
               model=provider["model"], effort=provider.get("effort", "high"), config_template=config_template)
    if source_manifest(destination / "upstream") != manifest:
        raise ValueError("Successor source differs from the ratified candidate; inspect source-input filtering")
    prep_state = read_json(destination / ".trellis/modeling/state.json")
    prep_state.update(next={"role": "reviewer"}, feedback=(
        "Implement the ratified source and requirement revision. Reuse the predecessor's extraction setup, "
        "adaptations and proofs where applicable, and check their applicability to this source. "
        "Read .trellis/modeling/revision-from.json and its read-only predecessor references. "
        "Original source fixes are already ratified; extraction adaptations must preserve the repaired program. "
        "The ordinary modeling human review must ratify any new Lean/NL meanings."))
    save_json(destination / ".trellis/modeling/state.json", prep_state)
    save_json(marker, receipt)  # Publication barrier after all initial state.
    return receipt


def continue_runtime(runtime: Path, *, resume: bool = False) -> bool:
    """Called by the native controller after the kernel's completed handoff."""
    if not read_handoff(runtime):
        return False
    from scripts import pv_native_create as native
    from trellis.pv_revision.driver import Driver
    from trellis.pv_revision.validation import ProductionChecker
    owner = native.runtime_job(runtime)
    if not owner:
        return False  # Standalone runs use the explicit revision CLI.
    job_dir, _, repo = owner
    with native.job_lock(job_dir):
        job = read_json(job_dir / "job.json")
        root = revision_root(runtime)
        pending = job.get("revision", {})
        if Path(job["runtime_root"]).resolve() != runtime.resolve():
            if pending.get("root") == str(root) and pending.get("pending") and resume:
                (job_dir / "native-controller.pause").unlink(missing_ok=True)
                (root / "pause").unlink(missing_ok=True)
                complete_successor(job_dir, job)
            return True  # A retired generation's duplicate callback is inert.
        if (job_dir / "native-controller.pause").exists() and not resume:
            return True
        if (runtime / "pause_request.json").exists() and not resume:
            return True
        if resume:
            (job_dir / "native-controller.pause").unlink(missing_ok=True)
            (root / "pause").unlink(missing_ok=True)
        native.stop_retired_services(runtime)
        root = initialize_runtime(runtime)
        driver = Driver(root, checker=ProductionChecker(root, read_json(root / "state.json")["tools"]))
        while (driver.state["status"] == "active" and not (job_dir / "native-controller.pause").exists()
               and not (root / "pause").exists()):
            driver.step()
        if driver.state["status"] != "accepted_for_modeling":
            return True
        if not driver.state["proposed_goals"]:
            driver.state.update(status="all_requirements_retired", phase="complete")
            driver.save()
            return True
        generation = int(job.get("native_generation", 0)) + 1
        new_runtime = runtime.parent / f"{job['slug']}-r{generation:04d}-runtime"
        if new_runtime.exists():
            raise ValueError("Successor runtime path is already occupied")
        before = dict(job)
        config_template = successor_config(root, native.active_config_file(job_dir, job))
        job.update(native_generation=generation, runtime_root=str(new_runtime),
                   goal_collection_path=str(root / "approved-goals.json"),
                   config_template_path=str(config_template),
                   revision={"root": str(root), "pending": True, "predecessor_repo": str(repo),
                             "predecessor_runtime": str(runtime)})
        # A durable intent precedes preparation, publication, or any agent work.
        save_json(root / "predecessor-job.json", before)
        save_json(root / "approved-goals.json", approved_collection(driver.state))
        save_json(job_dir / "job.json", job)
        complete_successor(job_dir, job)
    return True


def complete_successor(job_dir: Path, job: dict) -> None:
    from scripts import pv_native_create as native
    from trellis.pv_revision.driver import Driver
    from trellis.modeling.native import export_directory
    root = Path(job["revision"]["root"])
    preparation, repo, runtime = native.active_paths(job_dir, job)
    alias, previous = Path(job["repo"]), Path(job["revision"]["predecessor_repo"])
    if not alias.is_symlink() or alias.resolve() not in {previous.resolve(), preparation.resolve(), repo.resolve()}:
        raise ValueError("The project alias changed during revision handoff")
    if (job_dir / "native-controller.pause").exists():
        raise ValueError("The revision controller is paused")
    prepare_successor(root, preparation, config_template=native.active_config_file(job_dir, job))
    native.check_halt(preparation)
    from trellis.runtime.bridge import any_halt_marker_present
    if any_halt_marker_present(runtime):
        raise ValueError("Resolve the successor runtime halt before continuing")
    if (job_dir / "native-controller.pause").exists():
        raise ValueError("The revision controller is paused")
    driver = Driver(root)
    driver.state.update(status="successor_preparing", successor={"repo": str(repo), "runtime": str(runtime)})
    driver.save()
    if alias.resolve() == previous.resolve():
        native.publish_alias(alias, preparation, previous=previous)
    receipt = export_directory(preparation, repo) / "prepared.json"
    if receipt.is_file() and (runtime / "protocol_state.json").is_file():
        native.finish_native_launch(job_dir, preparation, repo, runtime)
        if alias.resolve() == preparation.resolve():
            native.publish_alias(alias, repo, previous=preparation)
    elif alias.resolve() == preparation.resolve():
        (preparation / ".trellis-creating").touch()
        native.launch_preparation(job_dir, job, preparation, repo, alias, runtime)
    else:
        raise ValueError("Published successor lacks a sealed bootstrap receipt")
    job["revision"]["pending"] = False
    save_json(job_dir / "job.json", job)
    driver.state["status"] = "successor_started"
    driver.save()
