"""Real Lean source-path churn and exact host-artifact publication regression."""
from __future__ import annotations

import json
from pathlib import Path
import shutil
import subprocess
import threading

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker.server import CheckerServer
from trellis.dispatch import candidate, publication_artifacts
from trellis.supervisor_workspace import sync_supervisor_workspace


@pytest.fixture
def real_sandbox(monkeypatch):
    if not shutil.which("bwrap"):
        pytest.skip("requires bubblewrap")
    probe = subprocess.run(["bwrap", "--ro-bind", "/", "/", "/bin/true"],
                           capture_output=True, text=True, timeout=10)
    if probe.returncode:
        pytest.skip("bubblewrap namespaces unavailable: " + probe.stderr)
    from trellis.host_runtime import worker_elan_home
    elan_home = worker_elan_home()
    # No provider state is needed for these compiler-only disposable repos.
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: Path("/nonexistent-test-home")))
    monkeypatch.setenv("TRELLIS_LEAN_PARALLELISM", "2")
    monkeypatch.setenv("ELAN_HOME", str(elan_home))
    for name in ("TRELLIS_CHECKER_SOCKET", "TRELLIS_CHECK_JOBS_SOCKET", "LEAN_PATH",
                 "LEAN_SYSROOT", "TRELLIS_PINNED_LEAN_LIBRARIES", "TRELLIS_HB_OUT"):
        monkeypatch.delenv(name, raising=False)


@pytest.fixture
def lean_toolchain(real_sandbox):
    from trellis.host_runtime import worker_elan_home

    # Exercise the toolchain implicated in the incident; never install it.
    root = worker_elan_home() / "toolchains/leanprover--lean4---v4.30.0-rc1"
    if not all((root / "bin" / name).is_file() for name in ("lean", "lake", "leanchecker")):
        pytest.skip("requires installed Lean v4.30.0-rc1")
    return root


@pytest.fixture
def supervisor_sync(monkeypatch):
    # Exercise real source sync and script provisioning without copying or
    # building a native runtime: this test does not claim native issuance.
    monkeypatch.setattr("trellis.checking.materialize_project_runtime", lambda *args: None)
    return sync_supervisor_workspace


def write_project(repo):
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".trellis/scripts").mkdir(parents=True)
    (repo / ".trellis/scripts/check.py").write_text("# source-closure fixture\n")
    (repo / "lean-toolchain").write_text("leanprover/lean4:v4.30.0-rc1\n")
    (repo / "lakefile.toml").write_text('name = "pathfixture"\n[[lean_lib]]\nname = "Tablet"\n')
    (repo / "lake-manifest.json").write_text(json.dumps({
        "version": "1.1.0", "packagesDir": ".lake/packages", "packages": [],
        "name": "pathfixture", "lakeDir": ".lake",
    }))
    # Persist the input filename in a declaration to reliably reproduce the
    # actual path-bearing exported artifact, independent of pretty printers.
    elaborator = ('open Lean Elab Term\n'
                  'elab "sourceName%" : term => do pure (Lean.mkStrLit (← getFileName))\n')
    (repo / "Tablet/Preamble.lean").write_text("import Lean\n")
    (repo / "Tablet/Legacy.lean").write_text(
        "import Tablet.Preamble\n" + elaborator +
        "def recordedSource : String := sourceName%\n"
        "theorem legacyTrue : True := trivial\n")
    (repo / "Tablet/Split.lean").write_text(
        "module\npublic import Lean\n" + elaborator +
        "public def splitSource : String := sourceName%\n"
        "public meta def fixtureTerm : TermElabM Expr := pure (Lean.mkNatLit 37)\n"
        "private def hidden : Nat := 3\n"
        "public theorem splitTrue : True := trivial\n")
    (repo / "Tablet/Consumer.lean").write_text(
        "import Tablet.Legacy\ntheorem consumerTrue : True := legacyTrue\n")


def test_real_two_root_churn_and_exact_host_artifact_handoff(
    tmp_path, lean_toolchain, supervisor_sync, monkeypatch,
):
    workers = [tmp_path / "acceptance-one/repo", tmp_path / "canonical-longer/repo"]
    repos, bundles = [], []
    nodes = ["Preamble", "Legacy", "Split", "Consumer"]
    for worker in workers:
        write_project(worker)
        repo = Path(supervisor_sync(worker)["authoritative_repo_path"])
        repos.append(repo)
        result = obs.materialize_tablet_oleans(
            repo, nodes, timeout_secs=60, bwrap_role="lake_compiler")
        assert result["returncode"] == 0, result
        artifacts = repo / ".lake/build/lib/lean/Tablet"
        bundles.append({p.name: p.read_bytes() for p in artifacts.iterdir()
                        if ".olean" in p.name and not p.name.endswith(".srcclosure")
                        or p.suffix == ".ir" or p.name.endswith(".ir.hash")})
        assert {"Split.olean", "Split.olean.server", "Split.olean.private",
                "Split.ir", "Split.ir.hash"} <= bundles[-1].keys()
        for node in nodes:
            assert obs.olean_is_content_current(repo, node)
            assert obs.olean_has_current_kernel_replay(repo, node)
        assert (str(repo) + "/Tablet/Legacy.lean").encode() in bundles[-1]["Legacy.olean"]
        probe = obs._run_lake_command(repo, ["env", "printenv", "LEAN_PATH"],
                                      timeout_secs=20, bwrap_role="lake_compiler")
        assert probe["returncode"] == 0, probe
        assert str(repo / ".lake/build/lib/lean") in probe["stdout"]
    # The same sources really yield different exported bytes under the
    # current compiler policy. Do not normalize or substitute artifact hashes.
    assert (repos[0] / "Tablet/Legacy.lean").read_bytes() == (repos[1] / "Tablet/Legacy.lean").read_bytes()
    assert bundles[0]["Legacy.olean"] != bundles[1]["Legacy.olean"]
    assert bundles[0]["Split.olean.private"] != bundles[1]["Split.olean.private"]
    # These records exercise only the handoff's exact bundle precondition;
    # the native issuer remains the final authority over actual certificates.
    response = {"local_closure_revalidation": {"refreshed": [
        [node, {"node_certificate": {"artifact_bundle": obs._artifact_bundle(repos[0], node)}}]
        for node in nodes
    ]}}
    journal = tmp_path / "publication-journal"
    journal.mkdir()
    handoff = publication_artifacts.stage(workers[0], workers[1], journal, response)
    assert set(handoff["nodes"]) == set(nodes)
    calls = []
    run_lake = obs._run_lake_command
    def replay_only(repo, args, **kwargs):
        assert args[0] != "build", "artifact handoff must not re-elaborate sources"
        calls.append(list(args))
        return run_lake(repo, args, **kwargs)
    monkeypatch.setattr(obs, "_run_lake_command", replay_only)
    checkpoints = []
    # A crash can leave only the exported part of an owner installed. Replay
    # the retained packet before any ordinary authority is allowed to use it.
    def interrupt_install(stage):
        if stage == "artifact:lib/lean/Tablet/Legacy.olean":
            raise RuntimeError("crash partway through artifact installation")
    with pytest.raises(RuntimeError, match="crash partway"):
        publication_artifacts.install(workers[1], journal, handoff, interrupt_install)
    assert (repos[1] / ".lake/build/lib/lean/Tablet/Legacy.olean").read_bytes() == bundles[0]["Legacy.olean"]
    assert (repos[1] / ".lake/build/lib/lean/Tablet/Split.olean.private").read_bytes() == bundles[1]["Split.olean.private"]
    publication_artifacts.install(workers[1], journal, handoff, checkpoints.append)
    assert checkpoints
    assert any(args[:2] == ["env", "leanchecker"] for args in calls)
    for name, data in bundles[0].items():
        assert (repos[1] / ".lake/build/lib/lean/Tablet" / name).read_bytes() == data
    for node in nodes:
        assert obs.olean_is_content_current(repos[1], node)
        assert obs.olean_has_current_kernel_replay(repos[1], node)
        for entry in obs._read_olean_provenance(repos[1], node)["kernel_replay"]["trusted_direct_imports"]:
            if entry["module"].startswith("Tablet."):
                assert Path(entry["olean"]).is_relative_to(repos[1])
    # A real imported metaprogram needs the module's runtime IR. It must be
    # usable after promotion, without borrowing an omitted canonical copy.
    probe = repos[1] / ".trellis/tmp/ImportedIR.lean"
    probe.write_text("import Lean\nimport Tablet.Split\nopen Lean Elab Term\n"
                     'elab "fromSplit%" : term => fixtureTerm\n'
                     "def result : Nat := fromSplit%\n"
                     "def main : IO Unit := IO.println result\n")
    result = run_lake(repos[1], ["env", "lean", "--run", str(probe)],
                      timeout_secs=30, bwrap_role="lake_compiler")
    assert result["returncode"] == 0 and result["stdout"].strip() == "37", result
    # The ordinary Lake check must also stay warm, independently of Trellis's
    # readiness shortcut. Exact bytes and mtimes detect an actual recompile.
    artifacts = repos[1] / ".lake/build/lib/lean/Tablet"
    before_build = {name: (artifacts / name).stat().st_mtime_ns for name in bundles[0]}
    result = run_lake(repos[1], ["build", *["Tablet." + node for node in nodes]],
                      timeout_secs=60, bwrap_role="lake_compiler")
    assert result["returncode"] == 0, result
    assert "Built Tablet." not in result["stdout"], result
    assert before_build == {name: (artifacts / name).stat().st_mtime_ns for name in bundles[0]}
    assert bundles[0] == {name: (artifacts / name).read_bytes() for name in bundles[0]}
    # Recovery replays exact journal bytes idempotently using independent
    # inodes; it cannot turn the acceptance workspace into a writable cache.
    publication_artifacts.install(workers[1], journal, handoff, checkpoints.append)
    for suffix in (".olean", ".olean.server", ".olean.private", ".olean.srcclosure", ".ir", ".ir.hash"):
        source = repos[0] / (".lake/build/lib/lean/Tablet/Split" + suffix)
        destination = repos[1] / (".lake/build/lib/lean/Tablet/Split" + suffix)
        assert source.stat().st_ino != destination.stat().st_ino
        assert destination.stat().st_nlink == 1
    # The ordinary checker readiness guard should require no compiler:
    # the native path can now inspect exactly the accepted artifacts.
    checker = CheckerServer.__new__(CheckerServer)
    checker.supervisor_repo = repos[1]
    checker._sync_lock = threading.RLock()
    with monkeypatch.context() as patch:
        def unexpected_compile(*args, **kwargs):
            raise AssertionError("promotion caused redundant compilation/replay")
        patch.setattr(obs, "_run_lake_command", unexpected_compile)
        assert checker._ensure_closures_have_current_kernel_replay(nodes, timeout_secs=60) is None
    # Auxiliary IR is still exact journal data: missing or changed staged
    # bytes must be rejected before any canonical file is installed.
    ir_name = "lib/lean/Tablet/Split.ir"
    staged_ir = journal / "artifacts" / ir_name
    ir_bytes = staged_ir.read_bytes()
    for altered in (ir_bytes + b"corrupt", None):
        if altered is None:
            staged_ir.unlink()
        else:
            staged_ir.write_bytes(altered)
        with pytest.raises((candidate.CandidateError, FileNotFoundError)):
            publication_artifacts.install(workers[1], journal, handoff, checkpoints.append)
        assert (artifacts / "Split.ir").read_bytes() == ir_bytes
        staged_ir.write_bytes(ir_bytes)
    # If acceptance has no auxiliary IR, it cannot silently reuse canonical
    # runtime code. Promotion may decline the owner or leave Lake to rebuild;
    # a metaprogram import must not execute a stale surviving canonical IR.
    source_ir = repos[0] / ".lake/build" / ir_name
    source_ir.unlink()
    missing_ir_journal = tmp_path / "missing-ir-journal"
    missing_ir_journal.mkdir()
    missing_ir = publication_artifacts.stage(workers[0], workers[1], missing_ir_journal, response)
    if "Split" in missing_ir["nodes"]:
        assert missing_ir["nodes"]["Split"]["files"][ir_name] is None
        publication_artifacts.install(workers[1], missing_ir_journal, missing_ir, checkpoints.append)
        assert not (artifacts / "Split.ir").exists()
        result = run_lake(repos[1], ["env", "lean", "--run", str(probe)],
                          timeout_secs=30, bwrap_role="lake_compiler")
        assert result["returncode"] != 0, result
        # The ordinary compiler remains available for the safe cache miss.
        result = run_lake(repos[1], ["build", "Tablet.Split"],
                          timeout_secs=60, bwrap_role="lake_compiler")
        assert result["returncode"] == 0, result
        result = run_lake(repos[1], ["env", "lean", "--run", str(probe)],
                          timeout_secs=30, bwrap_role="lake_compiler")
        assert result["returncode"] == 0 and result["stdout"].strip() == "37", result
    source_ir.write_bytes(ir_bytes)
    publication_artifacts.install(workers[1], journal, handoff, checkpoints.append)
    # Artifact corruption and an incomplete split bundle are ineligible,
    # even when source bytes and the response's claimed digest are unchanged.
    private = repos[0] / ".lake/build/lib/lean/Tablet/Split.olean.private"
    original = private.read_bytes()
    private.write_bytes(original + b"corrupt")
    assert not obs.olean_has_current_kernel_replay(repos[0], "Split")
    corrupt_journal = tmp_path / "corrupt-journal"
    corrupt_journal.mkdir()
    rejected = publication_artifacts.stage(workers[0], workers[1], corrupt_journal, response)
    assert "Split" not in rejected["nodes"]
    private.write_bytes(original)
    server = repos[0] / ".lake/build/lib/lean/Tablet/Split.olean.server"
    server.unlink()
    incomplete_journal = tmp_path / "incomplete-journal"
    incomplete_journal.mkdir()
    rejected = publication_artifacts.stage(workers[0], workers[1], incomplete_journal, response)
    assert "Split" not in rejected["nodes"]
    # Import closure edits invalidate both the owner and its consumer. The
    # canonical install must not resurrect their old artifacts after sync.
    source = workers[1] / "Tablet/Legacy.lean"
    source.write_text(source.read_text().replace("import Tablet.Preamble", "import Tablet.Missing"))
    publication_artifacts.install(workers[1], journal, handoff, checkpoints.append)
    assert not obs.olean_is_content_current(repos[1], "Legacy")
    assert not obs.olean_is_content_current(repos[1], "Consumer")
    assert not (repos[1] / ".lake/build/lib/lean/Tablet/Legacy.olean").exists()


@pytest.mark.parametrize("component", ["", ".trellis", ".trellis/supervisor", ".trellis/supervisor/repo"])
@pytest.mark.parametrize("operation", ["stage", "install"])
def test_artifact_handoff_refuses_lexical_supervisor_symlink(tmp_path, monkeypatch, component, operation):
    worker = tmp_path / "worker"
    outside = tmp_path / "outside"
    outside.mkdir()
    sentinel = outside / "untouched"
    sentinel.write_text("host-data")
    alias = worker / component
    alias.parent.mkdir(parents=True, exist_ok=True)
    alias.symlink_to(outside, target_is_directory=True)

    def unexpected_sync(*args, **kwargs):
        raise AssertionError("supervisor symlink must be rejected before source sync")

    monkeypatch.setattr("trellis.supervisor_workspace.sync_supervisor_workspace", unexpected_sync)
    with pytest.raises(candidate.CandidateError, match="symlink"):
        if operation == "stage":
            publication_artifacts.stage(worker, tmp_path / "canonical", tmp_path / "journal", {})
        else:
            handoff = {"version": 1, "nodes": {"Split": {
                "files": dict.fromkeys(publication_artifacts.paths("Split")),
            }}}
            publication_artifacts.install(worker, tmp_path / "journal", handoff, lambda _: None)
    assert sentinel.read_text() == "host-data"
