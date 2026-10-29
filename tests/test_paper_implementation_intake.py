"""Paper intake preserves authority and rejects unsupported trust/source changes."""
import hashlib
import json
import shutil
import subprocess
from pathlib import Path
from types import SimpleNamespace

import pytest

from trellis.paper_implementation import (
    MANIFEST, LeanPreparationDriver, canonical_json, config_block, export_paper_inputs,
    initialize_rust_preparation, lean_lakefile, lean_lake_manifest, paper_digest, paper_input,
    preparation_paper_digest, validate_manifest, write_manifest,
    stage_paper_inputs,
)


def dump(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False))


def paper_bundle(tmp_path):
    bundle = tmp_path / "original"
    bundle.mkdir()
    (bundle / "main.tex").write_text("\\begin{theorem}\\label{thm:main}\nAlgorithm returns the correct answer.\nIts running time is linear.\n\\end{theorem}\n")
    return bundle


def index(bundle, route="lean_native"):
    return paper_input(bundle, "main.tex", [{
        "id": "thm:main", "source_path": "main.tex", "start_line": 1, "end_line": 4,
        "selected_text": "Algorithm returns the correct answer.",
        "excluded_clauses": [{"text": "Its running time is linear.", "reason": "Complexity deferred beyond v1"}],
    }], route=route,
        entry_points=[{"node_id": "Algorithm", "declaration": "Algorithm",
                       "source_path": "Tablet/Algorithm.lean" if route == "lean_native" else "crate/src/lib.rs"}],
        implementation_roots={"thm:main": ["Algorithm"]})


@pytest.fixture
def subject(tmp_path):
    bundle = paper_bundle(tmp_path)
    paper = index(bundle)
    repo = tmp_path / "repo"
    (repo / "paper").mkdir(parents=True)
    (repo / "paper/main.tex").write_bytes((bundle / "main.tex").read_bytes())
    (repo / "Tablet").mkdir()
    (repo / "Tablet/Algorithm.lean").write_text("-- [TABLET NODE: Algorithm]\ndef Algorithm (n : Nat) : Nat := n + 1\n")
    (repo / "lean-toolchain").write_text("leanprover/lean4:v4.30.0-rc1\n")
    (repo / "lakefile.toml").write_text(lean_lakefile(["Algorithm"]))
    (repo / "lake-manifest.json").write_text(lean_lake_manifest())
    block = write_manifest(repo, paper=paper, source_files=["Tablet/Algorithm.lean"],
                           build_files=["lean-toolchain", "lakefile.toml", "lake-manifest.json"])
    return repo, block


def edit_manifest(repo, update):
    path = repo / MANIFEST
    body = json.loads(path.read_text())
    update(body)
    dump(path, body)


def test_lean_identity_and_semantic_clause_are_separate(subject):
    repo, block = subject
    result = validate_manifest(repo, block)
    assert result["manifest"]["targets"][0]["excluded_clauses"]
    assert result["manifest"]["implementation_roots"] == {"thm:main": ["Algorithm"]}
    assert result["manifest_sha256"] == hashlib.sha256((repo / MANIFEST).read_bytes()).hexdigest()
    assert not (repo / "GOAL.md").exists()
    assert "approval" not in result


def trust_config():
    return {"protocol": "trellis-trust-v1", "seed_manifest_path": "seed.json",
            "seed_definition_bundle_path": "definitions.json", "evidence_tool_manifest_path": "tools.json",
            "evidence_tool_root_path": "tools", "launch_acknowledgment_path": "launch.json",
            "seed_transaction_id": "seed", "advance_gate_episode_id": "gate"}


def test_lean_rejects_pv_only_trust_profile_without_changing_it(subject):
    import copy
    repo, block = subject
    config = {"paper_implementation": block, "trust_base": trust_config()}
    original = copy.deepcopy(config)
    with pytest.raises(ValueError, match="protocol supports only program-verification"):
        validate_manifest(repo, config)
    assert config == original
    rust_block = {**block, "route": "rust_aeneas"}
    assert config_block({"paper_implementation": rust_block, "trust_base": trust_config()}) == rust_block


def test_lean_preparation_rejects_pv_only_trust_before_writes(tmp_path):
    from trellis.paper_implementation import initialize_lean_preparation
    bundle = paper_bundle(tmp_path)
    template = tmp_path / "trust.config.json"
    config = {"trust_base": trust_config()}
    dump(template, config)
    root = tmp_path / "preparation"
    with pytest.raises(ValueError, match="protocol supports only program-verification"):
        initialize_lean_preparation(root, paper_bundle=bundle, paper=index(bundle), tools={},
                                    model="fixture", effort="high", config_template=template)
    assert not root.exists()
    assert json.loads(template.read_text()) == config


@pytest.mark.parametrize("change", [{"packages": [{"name": "unapproved"}]}, {"packagesDir": "vendor"}, {"version": "unknown"}])
def test_lean_rejects_noncanonical_dependency_resolution(subject, change):
    repo, block = subject
    lock = repo / "lake-manifest.json"
    data = json.loads(lock.read_text())
    data.update(change)
    dump(lock, data)
    edit_manifest(repo, lambda m: m["files"].update({"lake-manifest.json": hashlib.sha256(lock.read_bytes()).hexdigest()}))
    with pytest.raises(ValueError, match="canonical dependency-free"):
        validate_manifest(repo, block)


@pytest.mark.parametrize("target_exists", [False, True])
def test_lean_missing_lock_initialization_cannot_follow_symlink(subject, tmp_path, target_exists):
    import shutil
    from trellis.paper_implementation import _lean_candidate_manifest
    repo, block = subject
    manifest = json.loads((repo / MANIFEST).read_text())
    root = tmp_path / "preparation"
    shutil.copytree(repo, root / "work/lean")
    paper = {key: manifest[key] for key in ("route", "paper_path", "paper_files", "files", "targets", "entry_points", "implementation_roots")}
    dump(root / "config.json", {"paper_input": paper})
    outside = tmp_path / "unrelated-target"
    if target_exists:
        outside.write_text("keep")
    lock = root / "work/lean/lake-manifest.json"
    lock.unlink()
    lock.symlink_to(outside)
    with pytest.raises(ValueError, match="may not be a symlink"):
        _lean_candidate_manifest(root)
    assert outside.read_text() == "keep" if target_exists else not outside.exists()


def test_unicode_bundle_digest_matches_compact_utf8_map():
    manifest = {"paper_files": ["paper/数学.tex"], "files": {"paper/数学.tex": "a" * 64}}
    expected = hashlib.sha256(('{"paper/数学.tex":"' + 'a' * 64 + '"}').encode()).hexdigest()
    assert paper_digest(manifest) == expected


def test_source_change_invalidates_subject(subject):
    repo, block = subject
    with (repo / "Tablet/Algorithm.lean").open("a") as stream:
        stream.write("\n-- new source generation\n")
    with pytest.raises(ValueError, match="evidence changed"):
        validate_manifest(repo, block)


@pytest.mark.parametrize("bad", ["../escape", "/absolute", "paper//main.tex", "paper/./main.tex", "paper\\main.tex"])
def test_noncanonical_paths_rejected(subject, bad):
    repo, block = subject
    with pytest.raises(ValueError, match="Noncanonical"):
        config_block({**block, "manifest": bad})


def test_symlink_paper_is_not_pinned_evidence(subject):
    repo, block = subject
    source = repo / "paper/main.tex"
    source.rename(repo / "paper/original.tex")
    source.symlink_to("original.tex")
    with pytest.raises(ValueError, match="symlinks"):
        validate_manifest(repo, block)


@pytest.mark.parametrize("source", [
    "partial def Algorithm (n : Nat) : Nat := Algorithm n\n",
    "unsafe def Algorithm (n : Nat) : Nat := n\n",
    "@[extern \"fast\"] def Algorithm (n : Nat) : Nat := n\n",
    "@[implemented_by fast] def Algorithm (n : Nat) : Nat := n\n",
    "def Algorithm (n : Nat) : Nat := n\nattribute [csimp] Algorithm\n",
    "axiom Algorithm : Nat → Nat\n",
    "import Mathlib\ndef Algorithm (n : Nat) : Nat := n\n",
])
def test_lean_runtime_escape_is_rejected_even_with_recomputed_hash(subject, source):
    repo, block = subject
    path = repo / "Tablet/Algorithm.lean"
    path.write_text(source)
    edit_manifest(repo, lambda m: m["files"].update({"Tablet/Algorithm.lean": hashlib.sha256(path.read_bytes()).hexdigest()}))
    with pytest.raises(ValueError, match="Unsupported|import requires|safe total"):
        validate_manifest(repo, block)


def test_runtime_configuration_cannot_substitute_other_entrypoint(subject):
    repo, block = subject
    path = repo / "lakefile.toml"
    path.write_text(path.read_text().replace('Tablet.Algorithm', 'Fast.Algorithm'))
    edit_manifest(repo, lambda m: m["files"].update({"lakefile.toml": hashlib.sha256(path.read_bytes()).hexdigest()}))
    with pytest.raises(ValueError, match="exact supported Lake"):
        validate_manifest(repo, block)


def test_no_new_unpinned_trust_basis(subject):
    repo, block = subject
    dump(repo / "APPROVED_AXIOMS.json", {"global": ["desired_correctness"]})
    with pytest.raises(ValueError, match="trust basis is not pinned"):
        validate_manifest(repo, block)


def test_cost_target_cannot_enter_active_inventory(subject):
    repo, block = subject
    edit_manifest(repo, lambda m: m["targets"][0].update(kind="running_time"))
    with pytest.raises(ValueError, match="Cost/running-time"):
        validate_manifest(repo, block)


def test_replacement_prose_does_not_count_as_original_evidence(subject):
    repo, block = subject
    edit_manifest(repo, lambda m: m["targets"][0].update(selected_text="The program never crashes."))
    with pytest.raises(ValueError, match="verbatim"):
        validate_manifest(repo, block)


def test_idealized_root_cannot_replace_actual_entrypoint(subject):
    repo, block = subject
    edit_manifest(repo, lambda m: m["implementation_roots"].update({"thm:main": ["IdealAlgorithm"]}))
    with pytest.raises(ValueError, match="actual entry-point"):
        validate_manifest(repo, block)


def test_goals_and_claimed_manual_approvals_are_rejected(subject):
    repo, block = subject
    with pytest.raises(ValueError, match="prose goal"):
        validate_manifest(repo, {"paper_implementation": block, "goal_file": "GOAL.md"})
    edit_manifest(repo, lambda m: m.update(route="rust_aeneas", modeling_choices=[{
        "node": "Support", "source_locator": "crate/src/lib.rs:1", "rationale": "missing binding",
        "evidence_path": "x.json", "consequential": True, "approved": True}]))
    with pytest.raises(ValueError, match="claimed approvals"):
        validate_manifest(repo, {**block, "route": "rust_aeneas"})


def test_rust_paper_only_preparation_creates_no_goal(tmp_path, monkeypatch):
    from trellis.modeling import driver
    bundle = paper_bundle(tmp_path)
    paper = index(bundle, "rust_aeneas")
    monkeypatch.setattr(driver, "prepare_environment", lambda *a: None)
    monkeypatch.setattr(driver, "write_scripts", lambda *a: None)
    root = tmp_path / "preparation"
    initialize_rust_preparation(root, paper_bundle=bundle, paper=paper, tools={}, model="test", effort="high")
    inputs = json.loads((root / "config.json").read_text())
    config = json.loads((root / "trellis.config.json").read_text())
    assert "goals" not in inputs and not (root / "goals.json").exists()
    assert not (root / "GOAL.md").exists()
    assert config["workflow"]["paper_tex_path"] == "paper/main.tex"
    assert (root / "work/crate/src/lib.rs").is_file()
    assert preparation_paper_digest(root) == paper_digest(paper)
    (root / "paper/main.tex").write_text("changed")
    with pytest.raises(ValueError, match="Original paper input changed"):
        preparation_paper_digest(root)


def lean_driver_root(tmp_path):
    root = tmp_path / "preparation"
    dump(root / ".trellis/modeling/state.json", {
        "verification_protocol": "paper-lean-preparation/v1", "stage": "lean_implementation",
        "status": "active", "sequence": 0, "next": {"role": "worker"},
        "audits": {}, "history": [], "observation": {}, "feedback": ""})
    return root


def test_lean_preparation_uses_existing_agent_roles_and_requires_checker(tmp_path, monkeypatch):
    monkeypatch.setattr("trellis.modeling.driver.check_halt", lambda root: None)
    monkeypatch.setattr("trellis.modeling.performance.current_basis", lambda root: "candidate")
    root = lean_driver_root(tmp_path)
    calls = []
    responses = iter([{"summary": "authored source"}, {"action": "audit"}, {"decision": "accept", "reason": "reviewed source"}])
    def agent(root, state, task):
        calls.append(task["role"])
        return next(responses)
    checked = {"passed": True, "subject_sha256": "a" * 64, "resource_identity": "b" * 64}
    for _ in range(3):
        state = LeanPreparationDriver(root, agents=agent, checker=lambda root: checked).step()
    assert calls == ["worker", "reviewer", "audit"]
    assert state["status"] == "active" and state["next"] == {"role": "performance"}
    assert state["audits"]["lean_implementation"]["basis"] == checked["subject_sha256"]
    assert "human_approved" not in state


def test_lean_preparation_failed_build_cannot_be_waived(tmp_path, monkeypatch):
    monkeypatch.setattr("trellis.modeling.driver.check_halt", lambda root: None)
    root = lean_driver_root(tmp_path)
    path = root / ".trellis/modeling/state.json"
    state = json.loads(path.read_text());state["next"] = {"role": "audit"};dump(path, state)
    with pytest.raises(ValueError, match="cannot waive"):
        LeanPreparationDriver(root, agents=lambda *a: {"decision": "accept"}, checker=lambda root: {"passed": False}).step()
    assert json.loads(path.read_text())["status"] == "active"


def test_config_mode_retains_math_resolver_and_paper_authority(subject, monkeypatch):
    from trellis.config import load_config
    repo, block = subject
    monkeypatch.setattr("trellis.config.resolve_main_result_targets_via_kernel", lambda **kwargs: {
        "targets": [{"tex_label": "thm:main", "start_line": 1, "end_line": 4}],
        "available_labels": ["thm:main"], "preview": []})
    config = {"repo_path": str(repo), "worker": {"provider": "codex"}, "reviewer": {"provider": "codex"},
              "paper_implementation": block, "tmux": {}, "workflow": {"paper_tex_path": "paper/main.tex"}}
    path = repo / "trellis.config.json";dump(path, config)
    loaded = load_config(path)
    assert loaded.paper_implementation["manifest"]["route"] == "lean_native"
    assert loaded.goal_file == loaded.workflow.paper_tex_path
    config["workflow"]["allowed_import_prefixes"] = ["Mathlib", "Fast"]
    dump(path, config)
    with pytest.raises(Exception, match="ordinary import policy"):
        load_config(path)


def test_native_lean_check_requires_real_compiled_archive(subject, tmp_path, monkeypatch):
    """Kernel response/support installer are mocked; Lake compiles real source."""
    import shutil
    import subprocess
    from trellis.paper_implementation import check_lean_preparation
    lean = shutil.which("lean")
    if lean is None or shutil.which("elan") is None:
        pytest.skip("Installed Lean toolchain unavailable")
    selected = subprocess.run(["elan", "which", "lean"], text=True, capture_output=True, timeout=10)
    if selected.returncode:
        pytest.skip("Installed Lean toolchain unavailable")
    lean = selected.stdout.strip()
    repo, block = subject
    manifest = json.loads((repo / MANIFEST).read_text())
    root = tmp_path / "compile-preparation"
    root.mkdir()
    shutil.copytree(repo / "paper", root / "paper")
    shutil.copytree(repo, root / "work/lean")
    paper = {key: manifest[key] for key in ("route", "paper_path", "paper_files", "files", "targets", "entry_points", "implementation_roots")}
    dump(root / "config.json", {"paper_input": paper, "tools": {"lean": lean, "lean_toolchain": (repo / "lean-toolchain").read_text().strip()}})
    seen = []
    def ordinary(candidate, output):
        seen.append(candidate)
        return {"status": "check_tablet_ok", "output": {"ok": True}}
    monkeypatch.setattr("trellis.paper_implementation._ordinary_lean_check", ordinary)
    # Load shared checker bindings before replacing the deployment helper.
    monkeypatch.setattr("trellis.modeling.native.write_scripts", lambda *a: None)
    monkeypatch.setattr("trellis.checking.write_scripts", lambda *a: None)
    checked = check_lean_preparation(root)
    assert checked["passed"], checked
    assert Path(checked["compiled_library"]).stat().st_size > 0
    assert checked["compiled_library_sha256"] == hashlib.sha256(Path(checked["compiled_library"]).read_bytes()).hexdigest()
    assert seen[0] == root / "work/lean"
    assert checked["build"]["argv"][-1] == "Tablet:static"
    from trellis.paper_implementation import build_native_subject
    repeated = build_native_subject(root / "work/lean", tools={"lean": lean, "lean_toolchain": (repo / "lean-toolchain").read_text().strip()})
    assert repeated == checked["build"]


def test_rust_export_derives_bindings_from_actual_extraction(tmp_path):
    bundle = paper_bundle(tmp_path)
    paper = index(bundle, "rust_aeneas")
    prep = tmp_path / "preparation"
    (prep / "paper").mkdir(parents=True)
    (prep / "paper/main.tex").write_bytes((bundle / "main.tex").read_bytes())
    dump(prep / "config.json", {"paper_input": paper})
    repo = tmp_path / "export"
    (repo / "crate/src").mkdir(parents=True)
    (repo / "crate/src/lib.rs").write_text("pub fn Algorithm(n: u32) -> u32 { n + 1 }\n")
    (repo / "lean-toolchain").write_text("pinned\n")
    (repo / "Tablet").mkdir()
    (repo / "Tablet/Algorithm.lean").write_text("namespace demo\ndef Algorithm (n : Nat) : Nat := n + 1\nend demo\n")
    config = {"workflow": {}, "pv_tablet": {"verification_targets": ["old-goal"], "extraction_models": [{
        "node": "Algorithm", "lean": "def Algorithm (n : Nat) : Nat := n + 1", "namespace_context": "namespace demo",
        "provenance": {"source_file": "src/lib.rs"}}]}}
    block = export_paper_inputs(prep, repo, config, source_files=["crate/src/lib.rs"], build_files=["lean-toolchain"])
    result = validate_manifest(repo, config)
    assert result["manifest"]["entry_points"] == [{"node_id": "Algorithm", "declaration": "demo.Algorithm", "source_path": "crate/src/lib.rs"}]
    assert config["pv_tablet"]["verification_targets"] == []
    assert "goal_file" not in config and not (repo / "GOAL.md").exists()
    config["pv_tablet"]["extraction_models"] = []
    with pytest.raises(ValueError, match="not an extracted node"):
        export_paper_inputs(prep, repo, config, source_files=["crate/src/lib.rs"], build_files=["lean-toolchain"])


@pytest.mark.parametrize("route", ["rust_aeneas", "lean_native"])
def test_references_survive_export_and_bind_paper_identity(tmp_path, route):
    bundle = paper_bundle(tmp_path)
    references = {"references/.gitignore": b"*\n",
                  "references/catalogue/table.csv": b"id,weight\r\n1,2\r\n",
                  "references/target/[catalogue].json": b'{"exact":true}\r\n',
                  "references/check.py": b"raise RuntimeError('reference scripts require deliberate use')\n",
                  "references/receipt.json": b'{"success":true}\n',
                  "TRELLIS_REFERENCES.json": b'{"note":"Independently generated catalogue and checking support"}\n'}
    for name, data in references.items():
        destination = bundle / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
    paper = index(bundle, route)
    prep, repo = tmp_path / "preparation", tmp_path / "export"
    shutil.copytree(bundle, prep / "paper")
    dump(prep / "config.json", {"paper_input": paper})
    source = "crate/src/lib.rs" if route == "rust_aeneas" else "Tablet/Algorithm.lean"
    (repo / source).parent.mkdir(parents=True)
    (repo / source).write_text("pub fn Algorithm(n: u32) -> u32 { n }\n" if route == "rust_aeneas"
                              else "-- [TABLET NODE: Algorithm]\ndef Algorithm (n : Nat) : Nat := n\n")
    (repo / "lean-toolchain").write_text("pinned\n")
    build_files = ["lean-toolchain"]
    config = {"workflow": {}}
    if route == "rust_aeneas":
        (repo / "Tablet").mkdir()
        (repo / "Tablet/Algorithm.lean").write_text("def Algorithm (n : Nat) : Nat := n\n")
        config["pv_tablet"] = {"extraction_models": [{"node": "Algorithm", "lean": "def Algorithm (n : Nat) : Nat := n",
                                                     "provenance": {"source_file": "src/lib.rs"}}]}
    else:
        (repo / "lakefile.toml").write_text(lean_lakefile(["Algorithm"]))
        (repo / "lake-manifest.json").write_text(lean_lake_manifest())
        build_files += ["lakefile.toml", "lake-manifest.json"]
    export_paper_inputs(prep, repo, config, source_files=[source], build_files=build_files)
    manifest = validate_manifest(repo, config)["manifest"]
    for name, data in references.items():
        path = "paper/" + name
        assert (repo / path).read_bytes() == data
        assert path in manifest["paper_files"] and path not in manifest["source_files"]
        assert manifest["files"][path] == hashlib.sha256(data).hexdigest()
    assert paper_digest(manifest) == preparation_paper_digest(prep)
    assert manifest["targets"] == paper["targets"]
    assert not manifest["modeling_choices"] and not (repo / "APPROVED_AXIOMS.json").exists()
    from trellis.git_ops import init_repo, _git
    init_repo(repo)
    _git(repo, "add", "-A")
    stage_paper_inputs(repo, manifest)
    _git(repo, "commit", "-qm", "Pinned paper inputs")
    snapshot = tmp_path / "snapshot"
    subprocess.run(["git", "clone", "--quiet", str(repo), str(snapshot)], check=True, capture_output=True)
    for name in manifest["paper_files"]:
        assert (snapshot / name).read_bytes() == (repo / name).read_bytes()
    assert validate_manifest(snapshot, config)["manifest"] == manifest
    (repo / "paper/references/catalogue/table.csv").write_text("changed reference")
    with pytest.raises(ValueError, match="changed|mismatch"):
        validate_manifest(repo, config)


def test_preparation_snapshot_keeps_ignored_reference_bytes(tmp_path, monkeypatch):
    from trellis.modeling import environment
    bundle = paper_bundle(tmp_path)
    (bundle / ".gitignore").write_text("*\n")
    (bundle / "references/target").mkdir(parents=True)
    (bundle / "references/.gitignore").write_text("*\n")
    (bundle / "references/target/catalogue.json").write_bytes(b'{"exact":true}\r\n')
    paper = index(bundle)
    repo = tmp_path / "preparation"
    shutil.copytree(bundle, repo / "paper")
    dump(repo / "config.json", {"paper_input": paper})
    dump(repo / "paper-targets.json", paper)
    dump(repo / "trellis.config.json", {})
    dump(repo / ".trellis/modeling/state.json", {})
    for directory in ("work", "upstream"):
        (repo / directory).mkdir()
        (repo / directory / "initial.txt").write_text("fixture")
    monkeypatch.setattr(environment, "identify_tools", lambda _: {})
    environment.prepare_environment(repo, {})
    snapshot = tmp_path / "preparation-clone"
    subprocess.run(["git", "clone", "--quiet", str(repo), str(snapshot)], check=True, capture_output=True)
    for name in paper["paper_files"]:
        assert hashlib.sha256((snapshot / name).read_bytes()).hexdigest() == paper["files"][name]


def test_paper_staging_rejects_git_byte_conversion(subject):
    from trellis.git_ops import init_repo
    repo, block = subject
    manifest = json.loads((repo / MANIFEST).read_text())
    (repo / "paper/main.tex").write_bytes(b"Original paper\r\n")
    manifest["files"]["paper/main.tex"] = hashlib.sha256(b"Original paper\r\n").hexdigest()
    init_repo(repo)
    (repo / ".git/info/attributes").write_text("paper/** text eol=lf\n")
    with pytest.raises(ValueError, match="Git snapshot changed original paper bytes"):
        stage_paper_inputs(repo, manifest)
    assert (repo / "paper/main.tex").read_bytes() == b"Original paper\r\n"


@pytest.mark.parametrize("name", [".git/config", "nested/.GiT/config", ".gitattributes", "nested/.GITATTRIBUTES"])
def test_paper_input_rejects_git_control_paths(tmp_path, name):
    bundle = paper_bundle(tmp_path)
    path = bundle / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("reference")
    with pytest.raises(ValueError, match="Reserved Git control path"):
        index(bundle)


def test_wrong_workflow_locator_rejected(subject):
    repo, block = subject
    config = {"paper_implementation": block, "workflow": {"paper_tex_path": "paper/main.tex", "main_result_targets": [
        {"tex_label": "thm:main", "start_line": 2, "end_line": 4}]}}
    with pytest.raises(ValueError, match="target locators differ"):
        validate_manifest(repo, config)


def test_lean_worker_scope_excludes_checker_and_build_configuration():
    from trellis.modeling.driver import worker_writable_paths
    assert worker_writable_paths("lean_implementation") == ("work/lean/Tablet",)


def test_build_log_symlink_cannot_write_outside_owned_output(subject, tmp_path):
    from trellis.paper_implementation import build_native_subject
    repo, block = subject
    digest = validate_manifest(repo, block)["manifest_sha256"]
    out = repo / ".trellis/scratch/paper-native-build" / digest
    out.mkdir(parents=True)
    outside = tmp_path / "untouched.txt"
    outside.write_text("keep")
    (out / "build.log").symlink_to(outside)
    binaries = tmp_path / "tools/bin"
    binaries.mkdir(parents=True)
    for name in ("lean", "lake"):
        (binaries / name).write_text("not executed")
    with pytest.raises(ValueError, match="unlinked ordinary file"):
        build_native_subject(repo, tools={"lean": str(binaries / "lean"), "lean_toolchain": "leanprover/lean4:v4.30.0-rc1"})
    assert outside.read_text() == "keep"


def test_rust_preparation_prompt_has_single_gate_paper_process(tmp_path, monkeypatch):
    from trellis.modeling import driver
    bundle = paper_bundle(tmp_path)
    paper = index(bundle, "rust_aeneas")
    monkeypatch.setattr(driver, "prepare_environment", lambda *a: None)
    monkeypatch.setattr(driver, "write_scripts", lambda *a: None)
    root = tmp_path / "preparation"
    initialize_rust_preparation(root, paper_bundle=bundle, paper=paper, tools={}, model="test", effort="high")
    state = json.loads((root / ".trellis/modeling/state.json").read_text())
    (root / ".trellis/modeling/requests").mkdir()
    prompt = driver.agent_prompt(root, state, {"role": "worker"}, SimpleNamespace(raw=root / "raw.json", done=root / "done"))
    reference = (root / ".trellis/modeling/process.md").read_text()
    assert "exactly one human checkpoint" in reference
    assert "before proof formalization" in reference
    assert "GOAL.md" not in prompt and "GOAL.md" not in reference
    assert "proof, disproof, and classification belong" not in prompt
    assert "false or unsupported claims" in prompt


def test_lean_paper_only_init_removes_rust_phase_routing(tmp_path, monkeypatch):
    from trellis.paper_implementation import initialize_lean_preparation
    from trellis.modeling.driver import REPO
    bundle = paper_bundle(tmp_path)
    paper = index(bundle)
    template = json.loads((REPO / "examples/trellis.config.json").read_text())
    template.update(modeling={"enabled": True, "preservation": True}, pv_tablet={"language": "rust"}, goal_file="GOAL.md")
    path = tmp_path / "template.json";dump(path, template)
    monkeypatch.setattr("trellis.modeling.environment.prepare_environment", lambda *a: None)
    # Native keeps an imported binding; restore both after this fixture.
    monkeypatch.setattr("trellis.modeling.native.write_scripts", lambda *a: None)
    monkeypatch.setattr("trellis.checking.write_scripts", lambda *a: None)
    root = tmp_path / "lean-preparation"
    initialize_lean_preparation(root, paper_bundle=bundle, paper=paper, tools={"lean": "/bin/true", "lean_toolchain": "installed-test"},
                                config_template=path, model="test", effort="high")
    config = json.loads((root / "trellis.config.json").read_text())
    assert not {"modeling", "pv_tablet", "goal_file"} & config.keys()
    assert config.get("trust_base") == template.get("trust_base")
    assert not (root / "GOAL.md").exists()
    assert (root / "work/lean/Tablet/Preamble.lean").read_text() == ""
    state = json.loads((root / ".trellis/modeling/state.json").read_text())
    assert state["next"]["role"] == "worker" and state["stage"] == "lean_implementation"


@pytest.mark.parametrize("returncode", [0, 1])
def test_preparation_ordinary_check_owns_scoped_checker_and_has_no_fallback(tmp_path, monkeypatch, returncode):
    import os
    from trellis.paper_implementation import _ordinary_lean_check
    calls = []
    class Server:
        def __init__(self, runtime, **kwargs):
            assert json.loads((runtime / "runtime_metadata.json").read_text())["repo_path"] == str(tmp_path)
            self.socket_path = runtime / "checker.sock"
            self.log_path = runtime / "log"
        def set_expected_peer_uid(self, uid):
            assert uid == os.geteuid()
        def start(self): calls.append("start")
        def serve_forever(self): pass
        def shutdown(self): calls.append("shutdown")
    monkeypatch.setattr("trellis.checker.server.CheckerServer", Server)
    monkeypatch.setattr("trellis.supervisor_workspace.sync_supervisor_workspace", lambda path: calls.append("mirror"))
    monkeypatch.setattr("trellis.runtime.kernel_cli.kernel_cli_command", lambda: ["existing-runtime-cli"])
    monkeypatch.setenv("TRELLIS_CHECKER_SOCKET", "parent-socket")
    monkeypatch.setenv("TRELLIS_CHECKER_TOKEN", "parent-token")
    def run(command, **kwargs):
        calls.append(command)
        assert kwargs["env"]["TRELLIS_CHECKER_SOCKET"] != "parent-socket"
        assert "TRELLIS_CHECKER_TOKEN" not in kwargs["env"]
        assert json.loads(kwargs["input"])["node_role"] == {}
        assert kwargs["timeout"] == 180
        return SimpleNamespace(returncode=returncode, stdout='{"status":"check_tablet_ok"}', stderr="failure" if returncode else "")
    monkeypatch.setattr("trellis.paper_implementation.subprocess.run", run)
    if returncode:
        with pytest.raises(ValueError, match="Ordinary kernel check failed"):
            _ordinary_lean_check(tmp_path, tmp_path)
    else:
        assert _ordinary_lean_check(tmp_path, tmp_path)["status"] == "check_tablet_ok"
    assert calls == ["mirror", "start", ["existing-runtime-cli"], "shutdown"]
    assert os.environ["TRELLIS_CHECKER_SOCKET"] == "parent-socket"
    assert os.environ["TRELLIS_CHECKER_TOKEN"] == "parent-token"
