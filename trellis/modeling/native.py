"""Bootstrap audited extraction into the shared modeling runtime."""
from __future__ import annotations

import json
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import shlex
import time

from trellis.modeling.driver import REPO, setup_fingerprint, source_fingerprint
from trellis.modeling.environment import environment_signature, identify_tools
from trellis.modeling.observations import read_json, write_json
from trellis.checking import write_scripts
from trellis.project_paths import project_state_dir_for_repo
from trellis.pv_extraction_config import extraction_arguments
from trellis.runtime.kernel_cli import run_kernel_cli
from trellis.pv_execution import TIMEOUT_ENV, semantic_tools, run_owned_process
from trellis.pv_toolchain import (bootstrap_configuration_errors, check_library_resolution,
                                 effective_tools, lean_imports, native_resource_environment, native_resources, NativeLibraryEnvironmentError)


def export_directory(root: Path, destination: Path) -> Path:
    """Locate bootstrap output independently of the chosen preparation name."""
    identity = hashlib.sha256(str(destination.resolve()).encode()).hexdigest()[:12]
    return root / ".trellis/modeling/exports" / f"{destination.name}-{identity}"


class NativeExportCandidateError(ValueError):
    """An extraction candidate needs revision before native handoff."""


def validate_export(observation: dict, output: Path) -> dict:
    """Exercise the native splitter and manifest builders on the audited bytes."""
    from scripts.extract_pv_model import split_generated_lean, build_model_refinement_manifest, build_tcb_manifest
    from trellis.modeling.modules import imports, order_module_imports, ModuleDependencyCycle

    output.mkdir(parents=True, exist_ok=True)
    report = {"passed": False, "error": "", "report": str(output / "export.json")}
    try:
        modules = observation.get("raw_models", {})
        if len(modules) != 1:
            raise ValueError("Native export requires one extracted Lean module")
        source = Path(next(iter(modules.values())))
        raw = source.read_bytes()
        report.update(source=str(source), sha256=hashlib.sha256(raw).hexdigest())
        split = split_generated_lean(raw.decode("utf-8"), expose_types=True)
        report["nodes"] = [{"name": node.name, "deps": node.deps} for node in split.nodes]
        # Inspect emitted imports, not a claim about semantic dependencies.
        # Missing primitive support cannot repair a cycle among these modules.
        graph = {"Tablet.Preamble": imports(split.preamble_text),
                 **{f"Tablet.{node.name}": imports(node.text) for node in split.nodes}}
        try:
            order_module_imports(graph)
        except ModuleDependencyCycle as error:
            report.update(kind="candidate", cycle=error.cycle)
            raise NativeExportCandidateError(
                f"{error}. Revise the adaptation or request review of the extraction setup; "
                "missing model support cannot resolve cyclic generated imports. "
                "Do not repair frozen declarations through header macros. "
                f"Evidence: {output / 'export.json'}") from error
        write_json(output / "model-refinement.json", build_model_refinement_manifest(split))
        tcb = build_tcb_manifest(split)
        write_json(output / "tcb.json", tcb)
        from trellis.modeling.axioms import compatibility, PreparationAxiomTimeout
        try:
            report["axiom_compatibility"] = compatibility(observation, tcb, output=output / "axioms.json")
        except PreparationAxiomTimeout as error:
            report["kind"] = "candidate"
            raise NativeExportCandidateError(str(error)) from error
        blockers = report["axiom_compatibility"]["unapproved_axioms"]
        if blockers:
            report["kind"] = "candidate"
            raise NativeExportCandidateError(
                "Extracted declarations reach unapproved axioms: " + ", ".join(blockers)
                + ". Revise the adaptation or request review of the extraction setup. "
                "Missing model support cannot repair these already elaborated dependencies. "
                "Do not add discovered axioms to the approval policy. "
                f"Full dependency evidence: {output / 'axioms.json'}")
        report["passed"] = True
    except (ValueError, OSError, SystemExit) as error:
        report["error"] = str(error)
        raise
    finally:
        write_json(output / "export.json", report)
    return {key: report[key] for key in ("passed", "source", "sha256", "report", "axiom_compatibility")}


def materialize_model_layout(repo: Path, *, paper_implementation: bool = False) -> None:
    """Package audited declarations before initializing native runtime state.

    This changes file dependencies, not the source/extraction basis. Extracted
    types use the same pinned-node registry as extracted functions.
    """
    from scripts.extract_pv_model import (split_generated_lean, build_model_refinement_manifest,
                                         _empty_assumptions_lean, _empty_assumptions_tex)
    from trellis.pv_model_support import TYPE_LAYOUT, sync_model_support

    raw = (repo / "generated.lean").read_text()
    split = split_generated_lean(raw, expose_types=True)
    config = read_json(repo / "trellis.config.json")
    # Paper legacy mode has no UnderModelAssumptions role. Remove only the
    # extractor's byte-exact empty scaffold, never validity definitions or
    # authored premises. Required-v1 owns its authenticated seed support and
    # retains the existing packaging path unchanged.
    paper_legacy = paper_implementation and config.get("trust_base") is None
    remove_scaffolds = []
    if paper_legacy:
        if split.validity_definitions:
            raise ValueError("Paper legacy packaging cannot discard extracted validity definitions")
        scaffolds = [(repo / "Tablet/Assumptions.lean", _empty_assumptions_lean().encode()),
                     (repo / "Tablet/Assumptions.tex", _empty_assumptions_tex().encode())]
        present = [path.exists() or path.is_symlink() for path, _ in scaffolds]
        if any(present) and not all(present):
            raise ValueError("Paper legacy packaging requires the complete exact-empty assumptions scaffold pair")
        for (path, expected), exists in zip(scaffolds, present):
            if exists:
                if path.is_symlink() or not path.is_file() or path.read_bytes() != expected:
                    raise ValueError("Paper legacy packaging refuses nonempty or altered assumptions support")
                remove_scaffolds.append(path)
    pv = config["pv_tablet"]
    existing = {item["node"]: item for item in pv["extraction_models"]}
    source = next(iter(existing.values()))["provenance"]
    models = []
    for node in split.nodes:
        if node.name in existing:
            item = existing[node.name]
            if item["lean"] != node.region:
                raise ValueError(f"Packaging would change extracted declaration {node.name}")
        elif node.name in split.type_nodes:
            item = {"id": f"model:{node.name}", "node": node.name, "name": node.name,
                    "lean": node.region, "namespace_context": f"namespace {split.namespace}",
                    "provenance": source}
        else:
            raise ValueError(f"Packaging introduced an unexpected declaration {node.name}")
        models.append({**item, "model_dependencies": node.deps})
    if set(existing) - {node.name for node in split.nodes}:
        raise ValueError("Packaging would remove extracted declarations")
    for path in remove_scaffolds:
        path.unlink()
    pv["extraction_models"] = models
    pv["model_refinement"] = build_model_refinement_manifest(split)
    manifest = read_json(repo / "modeling-input/manifest.json")
    manifest["model_support_layout"] = TYPE_LAYOUT
    write_json(repo / "modeling-input/manifest.json", manifest)
    for node in split.nodes:
        prefix = "" if paper_legacy or node.name in split.type_nodes else "import Tablet.Assumptions\n"
        (repo / "Tablet" / f"{node.name}.lean").write_text(prefix + node.text)
    write_json(repo / "trellis.config.json", config)
    write_json(repo / "pv_tablet.json", pv)
    sync_model_support(repo, write=True, _paper_legacy_layout=paper_legacy)


def check_provenance(repo: Path, *, fresh: bool = False, output: Path | None = None,
                     bwrap_role: str | None = None) -> dict:
    """Check the audited preparation/export relationship using the extractor's digest semantics."""
    from scripts.extract_pv_model import crate_source_sha256, crate_source_tree_manifest, source_input_manifest
    from trellis.modeling.observations import observe, passed
    from trellis.atomic_actions.checker_client import CheckerRpcError

    output = output or repo / ".trellis/scratch" / f"provenance-{time.time_ns()}"
    output.mkdir(parents=True, exist_ok=False)
    result = {"checks": {}, "error": "", "report": str(output / "provenance.json")}
    checks = result["checks"]
    try:
        manifest = read_json(repo / "modeling-input/manifest.json")
        checks["provenance_schema"] = manifest.get("provenance_schema") == "trellis-native-preparation/v1"
        root = Path(manifest["preparation_root"])
        state = read_json(root / ".trellis/modeling/state.json")
        from trellis.project_paths import supervisor_workspace_cache_path
        selected_tools = effective_tools(root)
        resource_identity = environment_signature(
            selected_tools, cache_path=supervisor_workspace_cache_path(repo) / "resource-digests.json",
            read_caches=[root / ".trellis/modeling/cache/resource-digests.json"])
        setup = setup_fingerprint(root, resource_identity=resource_identity)
        source = source_fingerprint(root, setup=setup)
        for stage, basis in (("setup", setup), ("adaptation", source)):
            audit = state["audits"].get(stage, {})
            checks[f"{stage}_audit_current"] = (
                audit.get("decision") == "accept" and audit.get("basis") == basis
                and manifest["audits"].get(stage) == audit)
        checks["source_basis"] = manifest["source_basis"] == source
        checks["route_decisions"] = manifest.get("route_decisions", []) == state.get("route_decisions", [])
        checks["scope"] = manifest.get("scope", "") == state.get("scope", "")
        checks["declined_goals"] = manifest.get("declined_goals", {}) == state.get("declined", {})
        checks["resource_imports"] = manifest.get("lean_imports", []) == lean_imports(root)
        checks["effective_tools"] = semantic_tools(manifest.get("effective_tools", {})) == semantic_tools(selected_tools)
        checks["resource_identity"] = manifest.get("resource_identity") == resource_identity
        expected_identities = read_json(repo / "modeling-input/tool-identities.json")
        observed_identities = identify_tools(selected_tools)
        checks["tool_identities_current"] = expected_identities == observed_identities
        result["tool_identity_differences"] = {
            name: {"expected": expected_identities.get(name), "observed": observed_identities.get(name)}
            for name in sorted(expected_identities.keys() | observed_identities.keys())
            if (name not in expected_identities or name not in observed_identities
                or expected_identities[name] != observed_identities[name])
        }
        checks["resource_environment"] = manifest.get("resource_environment") == native_resource_environment(selected_tools)
        checks["lean_toolchain"] = (repo / "lean-toolchain").read_text().strip() == native_resources(selected_tools)["lean_toolchain"]
        result["library_resolution"] = check_library_resolution(
            repo, selected_tools, native_resource_environment(selected_tools), bwrap_role=bwrap_role)
        checks["library_resolution"] = result["library_resolution"]["matches"]
        checks["library_imports"] = (manifest["library_resolution"].get("approved_imports")
                                     == result["library_resolution"]["approved_imports"])
        checks["support_handoff"] = manifest.get("support_obligations", []) == state["audits"].get("adaptation", {}).get("support_obligations", [])
        if "axiom_compatibility_sha256" in manifest:
            digest = hashlib.sha256((repo / "modeling-input/axiom-compatibility.json").read_bytes()).hexdigest()
            checks["axiom_compatibility"] = (digest == manifest["axiom_compatibility_sha256"]
                == state["audits"].get("adaptation", {}).get("axiom_compatibility_sha256"))
        for name, prepared in (("crate", root / "work/crate"), ("upstream", root / "upstream")):
            checks[f"{name}_export"] = crate_source_tree_manifest(repo / name) == source_input_manifest(prepared)
        inputs = read_json(root / "config.json")
        referents = [("GOAL.md", "GOAL.md"), ("modeling-input/goals.json", "goals.json")]
        if "paper_input" in inputs:
            from trellis.paper_implementation import validate_manifest, preparation_paper_digest
            paper_subject = validate_manifest(repo, read_json(repo / "trellis.config.json"))
            checks["paper_bundle"] = paper_subject["paper_sha256"] == preparation_paper_digest(root)
            checks["paper_target_inventory"] = paper_subject["manifest"]["targets"] == inputs["paper_input"]["targets"]
            checks["paper_authority"] = not (repo / "GOAL.md").exists()
            from trellis.modeling import paper_choices
            checks["paper_modeling_discovery"] = paper_choices.check_export(root, state, repo)
            referents = [("modeling-input/paper-targets.json", "paper-targets.json")]
        for native, prepared in [*referents, ("modeling-input/extraction.json", "work/extraction.json"),
                                 ("modeling-input/tool-identities.json", "tool-identities.json")]:
            checks[f"export:{native}"] = (repo / native).read_bytes() == (root / prepared).read_bytes()
        source_sha = crate_source_sha256(repo / "crate")
        write_json(output / "source-tree.json", crate_source_tree_manifest(repo / "crate"))
        result["source_digest"] = {"scope": "canonical_crate_tree_manifest", "sha256": source_sha,
                                   "manifest": str(output / "source-tree.json")}
        for filename, pv in (("pv_tablet.json", read_json(repo / "pv_tablet.json")),
                             ("trellis.config.json", read_json(repo / "trellis.config.json")["pv_tablet"])):
            models = pv["extraction_models"]
            checks[f"source_digests:{filename}"] = all(
                model["provenance"]["source_sha256"] == source_sha for model in models)
        digests = read_json(repo / "tcb_manifest.json")["extraction_provenance"]["source_digests"]
        checks["source_digests:tcb_manifest.json"] = all(
            item["source_sha256"] == source_sha for item in digests)
        generated = manifest["generated_modules"]
        checks["audited_modules"] = generated == state["observation"]["raw_models"]
        if len(generated) != 1:
            raise ValueError("Native bootstrap requires one extracted module")
        native_bytes = (repo / manifest["native_model"]).read_bytes()
        checks["audited_model_export"] = native_bytes == Path(next(iter(generated.values()))).read_bytes()
        digest = hashlib.sha256(native_bytes).hexdigest()
        checks["bootstrap_receipt"] = manifest["bootstrap_reproduction"] == {
            "audited_sha256": digest, "native_sha256": digest, "byte_identical": True}
        from trellis.pv_model_support import sync_model_support
        if not (repo / "Tablet/model-support.json").is_file():
            raise ValueError("The native Modeling support manifest is missing")
        support = sync_model_support(repo)
        checks["model_support_composition"] = support["matches"]
        result["model_support"] = support
        if fresh:
            observation = observe(root, output / "extraction", cache_root=output / "cache", history_root=output)
            result["fresh_observation"] = observation["observation_file"]
            checks["fresh_extraction_succeeds"] = passed(observation, "aeneas")
            if not manifest.get("support_obligations"):
                checks["fresh_extraction_elaborates"] = passed(observation, "lean")
            fresh_modules = observation.get("raw_models", {})
            checks["fresh_model_export"] = (set(fresh_modules) == set(generated) and all(
                Path(path).read_bytes() == native_bytes for path in fresh_modules.values()))
    except NativeLibraryEnvironmentError as error:
        result["error"] = str(error)
        result["host_error"] = True
    except (OSError, ValueError, KeyError, RuntimeError, CheckerRpcError, SystemExit, subprocess.SubprocessError) as error:
        result["error"] = str(error)
    result["passed"] = not result["error"] and bool(checks) and all(checks.values())
    write_json(output / "provenance.json", result)
    return result


def launch_template(config: dict) -> dict:
    """Preserve native launcher settings; the standalone CLI retains its defaults."""
    template = json.loads(json.dumps(config.get("launch_config"))) if "launch_config" in config else read_json(REPO / "examples/trellis.config.json")

    def configure(value):
        if isinstance(value, dict):
            if "model" in value:
                value.update(provider="codex", model=config["model"], effort=config["effort"], label=config["model"])
            for child in value.values():
                configure(child)
        elif isinstance(value, list):
            for child in value:
                configure(child)

    if "launch_config" not in config:
        configure(template)
    template.pop("preparation", None)
    return template


def failed_bootstrap_output(root: Path, destination: Path, runtime: Path) -> Path | None:
    """Identify retryable output without modifying it or an initialized runtime."""
    if not any(p.exists() or p.is_symlink() for p in (destination, runtime)):
        return None
    state = read_json(root / ".trellis/modeling/state.json")
    failure = state.get("host_failure", {})
    staging = export_directory(root, destination)
    evidence = staging / "bootstrap-error.json"
    if failure.get("operation") != "native_bootstrap" or failure.get("evidence") != str(evidence):
        raise ValueError("Existing native output is not an attested failed bootstrap")
    record = read_json(evidence)
    if record.get("repo") != str(destination) or record.get("runtime") != str(runtime):
        raise ValueError("Failed bootstrap paths differ from the requested repository and runtime")
    if destination.is_symlink() or runtime.is_symlink():
        raise ValueError("Cannot preserve a failed bootstrap through an output symlink")
    if (runtime / "protocol_state.json").exists() or (runtime / "launch_env.json").exists():
        raise ValueError("An initialized native runtime requires runtime recovery, not preparation retry")
    return staging


def preserve_failed_bootstrap(root: Path, destination: Path, runtime: Path) -> Path | None:
    """Preserve only an attested failed bootstrap, before kernel initialization."""
    staging = failed_bootstrap_output(root, destination, runtime)
    if staging is None:
        return None
    archive = staging.with_name(staging.name + f".failed-{time.time_ns()}")
    staging.rename(archive)
    for path, name in ((destination, "partial-repository"), (runtime, "partial-runtime")):
        if path.exists():
            path.rename(archive / name)
    write_json(archive / "preserved.json", {"repo": str(destination), "runtime": str(runtime),
                                             "failure": read_json(archive / "bootstrap-error.json"),
                                             "preserved_at": time.time()})
    return archive


def initialize_native_runtime(destination: Path, runtime: Path, kernel_command: str) -> dict:
    # Kernel initialization already calls repo-local model-support checks.
    # Install their entry points and runtime snapshot before that first call.
    old = os.environ.get("TRELLIS_TRELLIS_KERNEL_CMD")
    os.environ["TRELLIS_TRELLIS_KERNEL_CMD"] = kernel_command
    try:
        write_scripts(destination, project_state_dir_for_repo(destination))
        return run_kernel_cli({"action": "init_from_config", "root": str(runtime),
                               "config_path": str(destination / "trellis.config.json")})
    finally:
        if old is None:
            os.environ.pop("TRELLIS_TRELLIS_KERNEL_CMD", None)
        else:
            os.environ["TRELLIS_TRELLIS_KERNEL_CMD"] = old


def prepare(root: Path, destination: Path, runtime: Path, *, prewarm_from: Path | None = None) -> Path:
    state = read_json(root / ".trellis/modeling/state.json")
    from trellis.modeling.engineering import validate_handoff
    validate_handoff(root, state)
    for stage, basis in (("setup", setup_fingerprint(root)), ("adaptation", source_fingerprint(root))):
        if (state["audits"].get(stage, {}).get("basis") != basis
                or state["audits"].get(stage, {}).get("decision") != "accept"):
            raise ValueError(f"Native modeling requires current {stage} audit approval")
    generated = state["observation"]["raw_models"]
    if len(generated) != 1:
        raise ValueError("The existing PV bootstrap currently requires a single extracted module")
    config = read_json(root / "config.json")
    if "paper_input" in config:
        from trellis.modeling.performance_loop import validate_acceptance
        validate_acceptance(root, state)
        from trellis.modeling import paper_choices
        paper_choices.validate_audit(root, state)
    tools = effective_tools(root)
    configuration_errors = bootstrap_configuration_errors(root, state["observation"])
    if configuration_errors:
        raise ValueError("; ".join(configuration_errors))
    resources = native_resources(tools)
    resource_environment = native_resource_environment(tools)
    resource_identity = environment_signature(tools, cache_path=root / ".trellis/modeling/cache/resource-digests.json")
    preserve_failed_bootstrap(root, destination, runtime)
    staging = export_directory(root, destination)
    staging.mkdir(parents=True, exist_ok=True)
    export = validate_export(state["observation"], staging / "validation")
    compatibility = export["axiom_compatibility"]
    if state["audits"]["adaptation"].get("axiom_compatibility_sha256") != compatibility["sha256"]:
        raise ValueError("Native preparation requires adaptation audit of the current axiom compatibility report")
    extraction = extraction_arguments(
        read_json(root / "work/extraction.json"),
        emit_json=read_json(root / "tool-identities.json").get("aeneas", {}).get("emit_json", False),
    )
    extraction["source_tools"] = tools
    extraction["preserve_history"] = True
    write_json(staging / "extraction-config.json", extraction)
    template = launch_template(config)
    write_json(staging / "config-template.json", template)
    aeneas_lib = Path(resources["aeneas_lean_lib"])
    lean_toolchain = resources["lean_toolchain"]
    launch_env = {
        **resource_environment,
        "TRELLIS_TRELLIS_KERNEL_CMD": (os.environ.get("TRELLIS_TRELLIS_KERNEL_CMD", "").strip()
                                       or str(REPO / "kernel/target/debug/trellis_runtime_cli")),
        "TRELLIS_SANDBOX_EXTRA_RO_PATHS": resource_environment["TRELLIS_SANDBOX_EXTRA_RO_PATHS"] + os.pathsep + str(root),
    }
    from trellis.modeling.reopening import reference_paths
    references = reference_paths(root)
    if references:
        launch_env["TRELLIS_SANDBOX_EXTRA_RO_PATHS"] += os.pathsep + os.pathsep.join(references)
    if TIMEOUT_ENV in os.environ:
        launch_env[TIMEOUT_ENV] = os.environ[TIMEOUT_ENV]
    command = [str(REPO / "scripts/setup_pv_repo.sh"), "--crate", str(root / "work/crate"),
               "--out-repo", str(destination), "--runtime-root", str(runtime),
               "--project-slug", destination.name, "--charon-bin", tools["charon"],
               "--aeneas-bin", tools["aeneas"], "--lean-toolchain", lean_toolchain,
               "--aeneas-lean-lib", str(aeneas_lib), "--skeleton-only", "--goal-mode", "prose",
               "--prepare-only", "--skip-corpus", "--config-template", str(staging / "config-template.json"),
               "--prewarm-library-only",
               "--extraction-config", str(staging / "extraction-config.json"),
               "--cargo-target-dir", str(staging / "cargo-target"), "--work-root", str(staging / "extraction")]
    if prewarm_from:
        command += ["--prewarm-from", str(prewarm_from)]
    from trellis.modeling.history import HISTORY, _tree, capture_inputs, record_bootstrap, record_model_baseline, start_attempt
    history_inputs = capture_inputs(root)
    history_inputs.update(_tree(root / "work/crate", "inputs/source", source=True))
    history_attempt = start_attempt(root / HISTORY, "native-bootstrap", inputs=history_inputs,
                                    metadata={"argv": command, "output_path": str(staging), "repo_path": str(destination)})
    completed = None
    try:
        with (staging / "setup.log").open("w") as log:
            completed = run_owned_process(command, cwd=REPO, env=dict(os.environ, **launch_env),
                                          stdout=log, stderr=subprocess.STDOUT)
    finally:
        record_bootstrap(root, staging, destination, inputs=history_inputs, argv=command,
                         exit_code=completed.returncode if completed is not None else None,
                         tool_identities=state["observation"].get("tool_identities", {}),
                         attempt_start_id=history_attempt)
    if completed.returncode:
        detail = (staging / "setup.log").read_text(errors="replace")[-4000:]
        raise RuntimeError(f"Native bootstrap exited {completed.returncode}; log: {staging / 'setup.log'}\n{detail}")
    native_bytes = (destination / "generated.lean").read_bytes()
    audited_bytes = Path(next(iter(generated.values()))).read_bytes()
    if native_bytes != audited_bytes:
        raise ValueError("The bootstrap extraction differs from the audited extraction")
    resolution = check_library_resolution(destination, tools, resource_environment, bwrap_role="lake_compiler")
    write_json(staging / "library-resolution.json", resolution)
    shutil.copytree(root / "upstream", destination / "upstream")
    bundle = destination / "modeling-input"
    bundle.mkdir()
    for name in ("reopened-from.json", "revision-from.json"):
        receipt = root / ".trellis/modeling" / name
        if receipt.is_file():
            shutil.copy2(receipt, bundle / name)
    shutil.copy2(compatibility["report"], bundle / "axiom-compatibility.json")
    for name in (("paper-targets.json", "tool-identities.json") if "paper_input" in config
                 else ("goals.json", "tool-identities.json")):
        shutil.copy2(root / name, bundle / name)
    if "paper_input" not in config:
        shutil.copy2(root / "GOAL.md", destination / "GOAL.md")
    shutil.copy2(root / "work/SETUP.md", bundle / "SETUP.md")
    shutil.copy2(root / "work/extraction.json", bundle / "extraction.json")
    write_json(bundle / "manifest.json", {
        "provenance_schema": "trellis-native-preparation/v1",
        "preparation_root": str(root), "audits": state["audits"],
        "route_decisions": state.get("route_decisions", []), "scope": state.get("scope", ""),
        "limitations": state.get("limitations", ""), "declined_goals": state.get("declined", {}),
        "preparation_layout": {"source": "work/crate", "configuration": "config.json",
                               "setup_notes": "work/SETUP.md"},
        "source_sha256_scope": "SHA-256 of the complete frozen extraction-input manifest; source_file locates a declaration within that tree",
        "source_basis": source_fingerprint(root), "generated_modules": generated,
        "lean_imports": lean_imports(root), "effective_tools": tools,
        "resource_identity": resource_identity, "resource_environment": resource_environment,
        "library_resolution": resolution,
        "support_obligations": state["audits"]["adaptation"].get("support_obligations", []),
        "axiom_compatibility_sha256": compatibility["sha256"],
        "model_support_schema": "trellis-model-support/v1",
        "preservation_schema": "trellis-preservation/v1",
        "bootstrap_reproduction": {"audited_sha256": hashlib.sha256(audited_bytes).hexdigest(),
                                   "native_sha256": hashlib.sha256(native_bytes).hexdigest(),
                                   "byte_identical": True},
        "native_model": "generated.lean", "extraction_config": "modeling-input/extraction.json",
        "provenance_check_command": shlex.join(["python3", str(destination / ".trellis/scripts/pv_modeling.py"),
                                                "check-native", str(destination)]),
        "fresh_extraction_check": shlex.join(["python3", str(destination / ".trellis/scripts/pv_modeling.py"),
                                             "check-native", str(destination), "--fresh"]),
    })
    from trellis.pv_model_support import empty_model_support, sync_model_support
    write_json(destination / "Tablet/model-support.json", empty_model_support())
    sync_model_support(destination, write=True)
    native_config = read_json(destination / "trellis.config.json")
    native_config["modeling"] = {"enabled": True, "preservation": True, "manifest": "modeling-input/manifest.json"}
    from trellis.paper_implementation import preparation_targets
    native_config["pv_tablet"]["verification_targets"] = ([] if "paper_input" in config else
        [g["id"] for g in preparation_targets(config) if g["id"] not in state.get("declined", {})])
    revision = template.get("pv_tablet", {}).get("revision")
    if revision is not None:
        native_config["pv_tablet"]["revision"] = revision
    if "paper_input" not in config:
        native_config["workflow"]["paper_tex_path"] = "GOAL.md"
    write_json(destination / "trellis.config.json", native_config)
    write_json(destination / "pv_tablet.json", native_config["pv_tablet"])
    if "paper_input" not in config:
        from trellis.modeling.goals import verify_inventory
        write_json(staging / "goal-inventory.json", verify_inventory(root, destination))
    materialize_model_layout(destination, paper_implementation="paper_input" in config)
    from trellis.modeling.preservation import preserve_successor_dag
    preserve_successor_dag(destination)
    record_model_baseline(root, destination)
    if "paper_input" in config:
        from trellis.paper_implementation import export_paper_inputs
        if state.get("declined"):
            raise ValueError("Paper v1 exports the selected original target set; change selection explicitly in a fresh preparation")
        native_config = read_json(destination / "trellis.config.json")
        source_files = sorted(p.relative_to(destination).as_posix() for p in (destination / "crate").rglob("*") if p.is_file())
        source_files.append("generated.lean")
        build_files = [name for name in ("lean-toolchain", "lakefile.lean", "lakefile.toml", "lake-manifest.json")
                       if (destination / name).is_file()]
        obligations = state["audits"]["adaptation"].get("support_obligations", [])
        from trellis.modeling import paper_choices
        discovery = paper_choices.export(root, state, destination)
        choices = discovery["choices"]
        if obligations and not choices:
            raise ValueError("Paper support handoff requires explicit extraction-model choices approved through preparation route review before export")
        if any(not choice.get("consequential") or choice.get("route_decision") is not None for choice in choices):
            raise ValueError("Planned paper modeling choices conservatively require fresh native route review; preparation cannot supply native approvals")
        write_json(bundle / "support-obligations.json", {
            "support_obligations": obligations, "adaptation_audit": state["audits"]["adaptation"],
            "raw_model": "generated.lean", "raw_model_sha256": hashlib.sha256(audited_bytes).hexdigest(),
            "planned_choices": choices,
            "approval": "Native route, Corr and preservation/Sound review required."})
        evidence_files = sorted(p.relative_to(destination).as_posix() for p in bundle.rglob("*") if p.is_file())
        from trellis.modeling.performance_loop import export_evidence
        evidence_files.extend(export_evidence(root, state, destination))
        from trellis.modeling.engineering import export_evidence as export_engineering_evidence
        evidence_files.extend(export_engineering_evidence(root, state, destination))
        export_paper_inputs(root, destination, native_config, source_files=source_files,
                            build_files=build_files, evidence_files=evidence_files, modeling_choices=choices)
        write_json(destination / "trellis.config.json", native_config)
        write_json(destination / "pv_tablet.json", native_config["pv_tablet"])
    (destination / "FAITHFULNESS.md").write_text(
        (REPO / "trellis/prompt_fragments/canonical/FAITHFULNESS.md").read_text().rstrip()
        + ("\n"
           if "paper_input" in config else "\n\n" + (REPO / "trellis/prompt_fragments/pv/modeling/faithfulness.md").read_text()))
    shutil.copy2(REPO / ("trellis/prompt_fragments/canonical/SUBSTANTIVENESS.md" if "paper_input" in config
                       else "trellis/prompt_fragments/pv/decide/substantiveness.md"), destination / "SUBSTANTIVENESS.md")
    # Config must be complete before the shared kernel seeds any state.
    result = initialize_native_runtime(destination, runtime, launch_env["TRELLIS_TRELLIS_KERNEL_CMD"])
    if result.get("status") != "ok":
        raise ValueError(json.dumps(result))
    # Initialization materializes the TCB provenance from the parsed config.
    # Validate that complete export before committing it or launching agents.
    provenance = check_provenance(destination, bwrap_role="lake_compiler")
    if not provenance["passed"]:
        failed = [name for name, passed in provenance["checks"].items() if not passed]
        detail = "; ".join([*failed, provenance.get("error", "")]).strip("; ")[:2000]
        raise ValueError(f"Native preparation provenance failed: {detail}. Report: {provenance['report']}")
    subprocess.run(["git", "add", "-A"], cwd=destination, check=True)
    if "paper_input" in config:
        from trellis.paper_implementation import MANIFEST, stage_paper_inputs
        stage_paper_inputs(destination, json.loads((destination / MANIFEST).read_text()))
    subprocess.run(["git", "commit", "-m", "Initialize native modeling from audited extraction"],
                   cwd=destination, stdout=subprocess.DEVNULL, check=True)
    write_json(staging / "prepared.json", {"repo": str(destination), "runtime": str(runtime),
                                            "launch_environment": launch_env})
    return destination / "trellis.config.json"
