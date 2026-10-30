"""Pinned Mathlib proof support remains separate from the core implementation."""
import json
import os
from pathlib import Path

import pytest

from trellis import paper_implementation as paper


def dump(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n")


def package(name, revision):
    return {"name": name, "url": paper.MATHLIB_URLS[name], "rev": revision, "type": "git",
            "subDir": None, "scope": "leanprover-community", "manifestFile": "lake-manifest.json",
            "inputRev": revision, "inherited": False, "configFile": "lakefile.toml"}


@pytest.fixture
def proof_subject(tmp_path):
    repo = tmp_path / "subject"
    (repo / "paper").mkdir(parents=True)
    (repo / "paper/main.tex").write_text("The algorithm returns its input.\n")
    (repo / "Tablet").mkdir()
    (repo / "Tablet/Algorithm.lean").write_text("-- [TABLET NODE: Algorithm]\ndef Algorithm (n : Nat) : Nat := n\n")
    (repo / "lean-toolchain").write_text("leanprover/lean4:v4.30.0-rc2\n")
    revision = "a" * 40
    upstream_lock = {**json.loads(paper.lean_lake_manifest()), "name": "mathlib",
                     "packages": [package("batteries", "b" * 40)]}
    root_lock = paper.mathlib_proof_lock(upstream_lock, revision)
    inventory = {"schema": "trellis-mathlib-proof-dependencies/v1",
                 "toolchain": "leanprover/lean4:v4.30.0-rc2", "packages": {}}
    for item in root_lock["packages"]:
        path = repo / ".lake/packages" / item["name"]
        path.mkdir(parents=True)
        (path / "Library.lean").write_text("-- Synthetic test package; no compilation/authentication claimed.\n")
        (path / "lean-toolchain").write_text("leanprover/lean4:v4.30.0-rc2\n")
        if item["name"] == "mathlib":
            dump(path / "lake-manifest.json", upstream_lock)
        inventory["packages"][item["name"]] = {"url": item["url"], "rev": item["rev"],
                                                "source_files": paper._package_source_files(path)}
    (repo / paper.NATIVE_LAKEFILE).parent.mkdir(parents=True)
    (repo / paper.NATIVE_LAKEFILE).write_text(paper.lean_lakefile(["Algorithm"]))
    (repo / paper.NATIVE_LOCK).write_text(paper.lean_lake_manifest())
    (repo / "lakefile.toml").write_text(paper.lean_proof_lakefile(["Algorithm"], revision))
    dump(repo / "lake-manifest.json", root_lock)
    dump(repo / paper.MATHLIB_LOCK, upstream_lock)
    dump(repo / paper.PROOF_INVENTORY, inventory)
    original = {"route": "lean_native", "paper_path": "paper/main.tex", "paper_files": ["paper/main.tex"],
                "targets": [{"id": "thm:main", "source_path": "paper/main.tex", "start_line": 1, "end_line": 1,
                             "selected_text": "The algorithm returns its input.", "excluded_clauses": []}],
                "entry_points": [{"node_id": "Algorithm", "declaration": "Algorithm", "source_path": "Tablet/Algorithm.lean"}],
                "implementation_roots": {"thm:main": ["Algorithm"]}}
    block = paper.write_manifest(repo, paper=original, source_files=["Tablet/Algorithm.lean"],
                                  build_files=["lean-toolchain", "lakefile.toml", "lake-manifest.json",
                                               paper.NATIVE_LAKEFILE, paper.NATIVE_LOCK, paper.MATHLIB_LOCK, paper.PROOF_INVENTORY],
                                  proof_project={"kind": "mathlib", "mathlib_rev": revision,
                                                 "dependency_inventory": paper.PROOF_INVENTORY})
    return repo, block


def test_mathlib_pins_do_not_change_implementation_roots(proof_subject):
    repo, block = proof_subject
    manifest = paper.validate_manifest(repo, block)["manifest"]
    assert manifest["source_files"] == ["Tablet/Algorithm.lean"]
    assert manifest["implementation_roots"] == {"thm:main": ["Algorithm"]}
    assert json.loads((repo / paper.NATIVE_LOCK).read_text())["packages"] == []
    assert {x["name"] for x in json.loads((repo / "lake-manifest.json").read_text())["packages"]} == {"mathlib", "batteries"}


@pytest.mark.parametrize("name", ["paper-implementation/native/lakefile.toml", "lake-manifest.json", "lean-toolchain"])
def test_pinned_build_or_toolchain_drift_is_rejected(proof_subject, name):
    repo, block = proof_subject
    with (repo / name).open("a") as stream:
        stream.write("\nchanged\n")
    with pytest.raises(ValueError, match="evidence changed"):
        paper.validate_manifest(repo, block)


@pytest.mark.parametrize("change", ["edit", "new_source", "package", "override"])
def test_proof_dependency_drift_is_rejected(proof_subject, change):
    repo, block = proof_subject
    if change == "edit":
        (repo / ".lake/packages/mathlib/Library.lean").write_text("axiom changed : False\n")
    elif change == "new_source":
        (repo / ".lake/packages/mathlib/Injected.lean").write_text("axiom changed : False\n")
    elif change == "package":
        (repo / ".lake/packages/extra").mkdir()
    else:
        dump(repo / ".lake/package-overrides.json", {})
    with pytest.raises(ValueError, match="source changed|unselected|overrides"):
        paper.validate_manifest(repo, block)


@pytest.mark.parametrize("change", [{"type": "path"}, {"url": "https://example.invalid/package"},
                                    {"rev": "main"}, {"name": "custom"}, {"subDir": "nested"}])
def test_arbitrary_dependency_expansion_is_rejected(proof_subject, change):
    repo, _ = proof_subject
    lock = json.loads((repo / paper.MATHLIB_LOCK).read_text())
    lock["packages"][0].update(change)
    with pytest.raises(ValueError, match="outside the pinned Mathlib"):
        paper.mathlib_proof_lock(lock, "a" * 40)


def test_repinning_root_lock_cannot_expand_original_mathlib_closure(proof_subject):
    repo, block = proof_subject
    path = repo / "lake-manifest.json"
    lock = json.loads(path.read_text())
    lock["packages"].append(package("aesop", "c" * 40))
    dump(path, lock)
    manifest = json.loads((repo / paper.MANIFEST).read_text())
    manifest["files"]["lake-manifest.json"] = paper.sha256(path)
    dump(repo / paper.MANIFEST, manifest)
    with pytest.raises(ValueError, match="closure differs"):
        paper.validate_manifest(repo, block)


def test_mathlib_seed_toolchain_must_match_before_materialization(tmp_path):
    seed = tmp_path / "seed"
    seed.mkdir()
    (seed / "lean-toolchain").write_text("wrong\n")
    with pytest.raises(ValueError, match="toolchains must match"):
        paper.prepare_mathlib_proof_project(tmp_path / "new", seed, ["Algorithm"], "selected")
    assert not (tmp_path / "new").exists()


@pytest.mark.parametrize("suffix", ["", ".git"])
def test_official_seed_url_spellings_reach_the_same_canonical_proof_lock(tmp_path, monkeypatch, suffix):
    seed = tmp_path / "seed"
    (seed / ".lake/packages/mathlib").mkdir(parents=True)
    (seed / "lean-toolchain").write_text("fixture\n")
    dump(seed / "lake-manifest.json", {"packages": [{"name": "mathlib", "rev": "a" * 40,
        "url": "https://github.com/leanprover-community/mathlib4" + suffix}]})
    upstream = {**json.loads(paper.lean_lake_manifest()), "name": "mathlib"}
    dump(seed / ".lake/packages/mathlib/lake-manifest.json", upstream)
    seen = []
    actual = paper.mathlib_proof_lock

    def inspect(lock, revision):
        result = actual(lock, revision)
        seen.append(result["packages"][0]["url"])
        raise ValueError("fixture stops before trusted cache admission")

    monkeypatch.setattr(paper, "mathlib_proof_lock", inspect)
    with pytest.raises(ValueError, match="fixture stops"):
        paper.prepare_mathlib_proof_project(tmp_path / "destination", seed, ["Algorithm"], "fixture")
    assert seen == [paper.MATHLIB_URLS["mathlib"]]


def test_core_build_excludes_proof_nodes_packages_and_proof_configuration(proof_subject, tmp_path, monkeypatch):
    from trellis.modeling import environment
    repo, block = proof_subject
    binary = tmp_path / "toolchain/bin"
    binary.mkdir(parents=True)
    (binary / "lean").write_bytes(b"fixture compiler")
    (binary / "lake").write_bytes(b"fixture lake")
    monkeypatch.setattr(environment, "environment_signature", lambda *args, **kwargs: "fixture-resource")
    observations = []

    def compile_fixture(argv, *, cwd, env, stdout, timeout):
        assert not (cwd / ".lake/packages").exists()
        assert not (cwd / "Tablet/Correct.lean").exists()
        assert (cwd / "lakefile.toml").read_text() == paper.lean_lakefile(["Algorithm"])
        assert json.loads((cwd / "lake-manifest.json").read_text())["packages"] == []
        assert (cwd / "Tablet/Algorithm.lean").read_bytes() == (repo / "Tablet/Algorithm.lean").read_bytes()
        observations.append(sorted(p.relative_to(cwd).as_posix() for p in cwd.rglob("*") if p.is_file()))
        library = cwd / ".lake/build/lib/libTablet.a"
        library.parent.mkdir(parents=True)
        library.write_bytes(b"fixture compilation of unchanged core source")
        return {"exit_code": 0, "timeout": False}

    monkeypatch.setattr(environment, "run_source_command", compile_fixture)
    tools = {"lean": str(binary / "lean"), "lean_toolchain": "leanprover/lean4:v4.30.0-rc2"}
    before = paper.build_native_subject(repo, tools=tools)
    (repo / "Tablet/Correct.lean").write_text("import Mathlib.Data.Nat.Prime.Basic\n-- separate proof\n")
    after = paper.build_native_subject(repo, tools=tools)
    assert before == after
    assert observations[0] == observations[1]
    assert before["native_build_inputs"]["lakefile.toml"]["source_path"] == paper.NATIVE_LAKEFILE
    assert before["proof_support"]["dependency_inventory_sha256"] == paper.sha256(repo / paper.PROOF_INVENTORY)


def test_package_config_prewarm_detaches_seed_hardlinks_and_excludes_candidate_sources(proof_subject, tmp_path, monkeypatch):
    from trellis.modeling import environment
    repo, _ = proof_subject
    seed = tmp_path / "seed"
    relative = ".lake/packages/mathlib/.lake/config/mathlib/lakefile.olean.trace"
    original = seed / relative
    original.parent.mkdir(parents=True)
    original.write_text("trusted seed configuration at index 2")
    copied = repo / relative
    copied.parent.mkdir(parents=True)
    os.link(original, copied)
    assert copied.stat().st_nlink == 2
    monkeypatch.setattr(paper, "_installed_lean_tools", lambda _: {"lean": "/fixture/bin/lean", "lean_toolchain": "fixture"})

    def configure(argv, *, cwd, env, stdout, timeout):
        assert argv == ["/fixture/bin/lake", "--no-cache", "env", "/usr/bin/true"]
        assert not (cwd / "Tablet").exists()
        assert (cwd / "lakefile.toml").read_bytes() == (repo / "lakefile.toml").read_bytes()
        assert copied.stat().st_nlink == 1
        copied.write_text("new proof project configuration at index 1")
        assert original.read_text() == "trusted seed configuration at index 2"
        return {"exit_code": 0, "timeout": False}

    monkeypatch.setattr(environment, "run_source_command", configure)
    paper._prewarm_mathlib_configuration(repo, seed)
    report = json.loads((repo / ".trellis/scratch/proof-config-prewarm/result.json").read_text())
    assert report["seed_configuration_unchanged"]
    assert not report["candidate_sources_present"]
    assert report["seed_config_files"] != report["proof_config_files"]


@pytest.mark.skipif(not os.environ.get("TRELLIS_TEST_MATHLIB_PROJECT"),
                    reason="Requires an explicitly selected installed trusted Mathlib cache and current native checker")
def test_real_mathlib_proof_preserves_compiled_implementation(tmp_path):
    """Opt-in bounded compiler/checker integration; no semantic agent is invoked."""
    from trellis.checking import write_scripts
    seed = Path(os.environ["TRELLIS_TEST_MATHLIB_PROJECT"])
    repo = tmp_path / "real-proof-project"
    (repo / "paper").mkdir(parents=True)
    (repo / "paper/main.tex").write_text("\\begin{theorem}\\label{thm:main}\nThe algorithm returns a prime.\n\\end{theorem}\n")
    (repo / "Tablet").mkdir()
    (repo / "Tablet/Algorithm.lean").write_text("-- [TABLET NODE: Algorithm]\ndef Algorithm : Nat :=\n-- BODY\n2\n")
    (repo / "Tablet/Algorithm.tex").write_text("\\begin{definition}\nAlgorithm returns two.\n\\end{definition}\n")
    for name in ("Preamble.lean", "Preamble.tex"):
        (repo / "Tablet" / name).write_text("")
    toolchain = (seed / "lean-toolchain").read_text().strip()
    (repo / "lean-toolchain").write_text(toolchain + "\n")
    profile = paper.prepare_mathlib_proof_project(repo, seed, ["Algorithm"], toolchain)
    original = {"route": "lean_native", "paper_path": "paper/main.tex", "paper_files": ["paper/main.tex"],
                "targets": [{"id": "thm:main", "source_path": "paper/main.tex", "start_line": 1, "end_line": 3,
                             "selected_text": "The algorithm returns a prime.", "excluded_clauses": []}],
                "entry_points": [{"node_id": "Algorithm", "declaration": "Algorithm", "source_path": "Tablet/Algorithm.lean"}],
                "implementation_roots": {"thm:main": ["Algorithm"]}}
    block = paper.write_manifest(repo, paper=original, source_files=["Tablet/Algorithm.lean", "Tablet/Preamble.lean"],
                                  build_files=["lean-toolchain", "lakefile.toml", "lake-manifest.json",
                                               paper.NATIVE_LAKEFILE, paper.NATIVE_LOCK, paper.MATHLIB_LOCK, paper.PROOF_INVENTORY],
                                  proof_project=profile)
    config = json.loads((Path(__file__).parents[1] / "examples/trellis.config.json").read_text())
    config.update(repo_path=str(repo), paper_implementation=block)
    for field in ("goal_file", "pv_tablet", "modeling"):
        config.pop(field, None)
    config["workflow"].update(start_phase="theorem_stating", paper_tex_path="paper/main.tex",
                              main_result_targets=[{"tex_label": "thm:main", "start_line": 1, "end_line": 3}])
    config["workflow"].pop("main_result_labels", None)
    dump(repo / "trellis.config.json", config)
    write_scripts(repo, repo / ".trellis")
    source_output = repo / ".trellis/scratch/source-check"
    source_output.mkdir(parents=True)
    source_checked = paper._ordinary_lean_check(repo, source_output)
    dump(source_output / "ordinary-check.json", source_checked)
    assert source_checked["output"]["ok"], source_checked
    before = paper.build_native_subject(repo)
    (repo / "Tablet/PrimeCorrect.lean").write_text(
        "-- [TABLET NODE: PrimeCorrect]\nimport Tablet.Algorithm\nimport Mathlib.Data.Nat.Prime.Basic\n"
        "theorem PrimeCorrect : Nat.Prime Algorithm :=\n-- BODY\nby simpa [Algorithm] using Nat.prime_two\n")
    (repo / "Tablet/PrimeCorrect.tex").write_text(
        "\\begin{theorem}\nAlgorithm returns a prime.\n\\end{theorem}\n\\begin{proof}\n"
        "By the definition \\noderef{Algorithm}, the result is two, which is prime.\n\\end{proof}\n")
    output = repo / ".trellis/scratch/proof-check"
    output.mkdir(parents=True)
    checked = paper._ordinary_lean_check(repo, output)
    dump(output / "ordinary-check.json", checked)
    assert checked["output"]["ok"], checked
    assert "PrimeCorrect" in checked["output"]["nodes"]
    assert checked["output"]["nodes"]["PrimeCorrect"]["axioms_valid"]
    assert checked["output"]["nodes"]["PrimeCorrect"]["sorry_free"]
    after = paper.build_native_subject(repo)
    assert before == after
    dump(output / "native-before.json", before)
    dump(output / "native-after.json", after)
    assert not (repo / "GOAL.md").exists()
    from trellis.git_ops import init_repo, _git
    from trellis.modeling.native import initialize_native_runtime
    init_repo(repo)
    _git(repo, "add", "-A")
    _git(repo, "commit", "--quiet", "-m", "Initialize bounded Mathlib paper-mode admission fixture")
    runtime = repo.parent / "native-runtime"
    command = os.environ.get("TRELLIS_TRELLIS_KERNEL_CMD", str(Path(__file__).parents[1] / "kernel/target/debug/trellis_runtime_cli"))
    initialized = initialize_native_runtime(repo, runtime, command)
    dump(output / "native-init.json", initialized)
    assert initialized["status"] == "ok", initialized
    state = json.loads((runtime / "protocol_state.json").read_text())
    assert state["phase"] == "TheoremStating"
    assert state["paper_implementation"]["proof_support"] == after["proof_support"]
