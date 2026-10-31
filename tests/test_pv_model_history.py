import json
from pathlib import Path

import pytest

from trellis.modeling import history


def write(path: Path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value) if isinstance(value, dict) else value)


def observation(root, name="one", *, cached=False, failed=False):
    output = root / "observations" / name
    write(output / "source/lib.rs", "fn f() {}\n")
    write(output / "source/target/skip.o", "compiled")
    write(output / "Generated/m.llbc", "raw llbc")
    write(output / "Generated/M.lean", "namespace M\ndef f := 1\nend M\n")
    write(output / "Lean/M.lean", "import Aeneas\n")
    write(output / "Lean/M.olean", "compiled")
    result = {"steps": [{"name": "aeneas", "cached": cached, "exit_code": 1 if failed else 0,
                         "argv": ["aeneas", "-backend", "lean"]}],
              "error": "partial output" if failed else "", "tool_identities": {"aeneas": {"sha256": "tool"}},
              "observation_file": str(output / "observation.json")}
    write(output / "observation.json", result)
    return output, result


def native(root):
    write(root / "trellis.config.json", {"pv_tablet": {"extraction_models": [{"id": "model:M", "node": "M"}]}})
    write(root / "Tablet/model-support.json", {"schema": "trellis-model-support/v1", "imports": [], "bindings": []})
    write(root / "Tablet/M.lean", "import Tablet.Preamble\ndef f := 1\n")
    write(root / "Tablet/M.tex", "Meaning of f")
    write(root / "Tablet/Preamble.lean", "import Aeneas\n")
    write(root / "generated.lean", "def f := 1\n")
    write(root / "modeling-input/manifest.json", {"lean_imports": []})
    return root


def record_bytes(destination, reference):
    record = json.loads((destination / reference["record"]).read_text())
    return record, {entry["path"]: (destination / "blobs" / entry["sha256"]).read_bytes() for entry in record["files"]}


def test_captured_attempts_keep_exact_outputs_failures_cache_and_inputs(tmp_path):
    root = tmp_path / "prep"
    history.initialize_history(root)
    for name, failed, cached in (("one", False, False), ("two", True, False), ("three", False, True)):
        output, result = observation(root, name, cached=cached, failed=failed)
        history.record_observation(root, output, result, inputs={"inputs/work/extraction.json": b'{"cargo_args":[]}'})
    destination = tmp_path / "export"
    manifest = history.export_history(root, destination)
    assert manifest["history_complete"] is True
    observations = [r for r in manifest["records"] if r["kind"] == "observation"]
    assert len(observations) == 3
    for reference in observations:
        record, files = record_bytes(destination, reference)
        assert files["raw-generated/m.llbc"] == b"raw llbc"
        assert files["source/lib.rs"] == b"fn f() {}\n"
        assert "composed-lean/M.olean" not in files
        assert "source/target/skip.o" not in files
        assert record["metadata"]["observation"]["steps"][0]["argv"] == ["aeneas", "-backend", "lean"]
    assert any(record_bytes(destination, r)[0]["metadata"]["observation"]["steps"][0]["cached"] for r in observations)
    assert any(record_bytes(destination, r)[0]["metadata"]["observation"]["error"] for r in observations)
    # Re-export is deterministic even to another absolute destination.
    other = tmp_path / "other"
    assert history.export_history(root, other) == manifest
    assert (other / "manifest.json").read_bytes() == (destination / "manifest.json").read_bytes()


@pytest.mark.parametrize("corruption", ["blob", "record", "delete-record"])
def test_changed_sealed_history_fails_closed(tmp_path, corruption):
    root = tmp_path / "prep"
    output, result = observation(root)
    reference = history.record_observation(root, output, result, inputs={})
    store = root / history.HISTORY
    if corruption == "blob":
        next((store / "blobs").iterdir()).write_bytes(b"tampered")
    elif corruption == "record":
        (store / reference["record"]).write_text("{}")
    else:
        (store / reference["record"]).unlink()
    with pytest.raises(ValueError, match="changed"):
        history.export_history(root, tmp_path / "export")


def test_legacy_recovery_is_read_only_and_explicitly_partial(tmp_path):
    root = tmp_path / "prep"
    output, _ = observation(root)
    before = {str(p.relative_to(root)): p.read_bytes() for p in root.rglob("*") if p.is_file()}
    manifest = history.export_history(root, tmp_path / "export")
    after = {str(p.relative_to(root)): p.read_bytes() for p in root.rglob("*") if p.is_file()}
    assert before == after
    assert not manifest["history_complete"]
    assert manifest["records"][0]["coverage"] == "recovered_partial"
    record, files = record_bytes(tmp_path / "export", manifest["records"][0])
    assert "original tool invocation inputs" in record["metadata"]["limitation"]
    assert "inputs/config.json" not in files
    assert history.export_history(root, tmp_path / "none", recover_available=False)["coverage"] == "unavailable"


def test_packaging_baseline_process_changes_and_support_dependencies(tmp_path):
    root, repo = tmp_path / "prep", native(tmp_path / "native")
    history.record_model_baseline(root, repo)
    write(repo / "Tablet/M.tex", "Refined meaning of f")
    write(repo / "Tablet/model-support.json", {"schema": "trellis-model-support/v1", "imports": ["Support"], "bindings": []})
    write(repo / "Tablet/Support.lean", "import Tablet.Helper\ndef support := helper\n")
    write(repo / "Tablet/Support.tex", "Support meaning")
    write(repo / "Tablet/Helper.lean", "def helper := 1\n")
    write(repo / "Tablet/Helper.tex", "Helper meaning")
    destination = tmp_path / "lineage"
    result = history.snapshot_model_lineage(repo, destination)
    assert result["history_complete"]
    changed = {entry["path"] for entry in result["changes"]}
    assert {"Tablet/M.tex", "Tablet/Support.lean", "Tablet/Helper.lean"} <= changed
    diff = (destination / result["diff"]).read_text()
    assert "-Meaning of f" in diff and "+Refined meaning of f" in diff
    _, raw = record_bytes(destination, result["baseline"])
    assert raw["generated.lean"] == b"def f := 1\n"
    assert "Tablet/Support.lean" not in raw


def test_missing_baseline_is_never_reconstructed_and_sealed_corruption_fails(tmp_path):
    repo = native(tmp_path / "native")
    result = history.snapshot_model_lineage(repo, tmp_path / "legacy")
    assert result["coverage"] == "current_only_baseline_unavailable"
    assert result["changes"] == [] and result["baseline"] is None
    history.record_model_baseline(tmp_path / "prep", repo)
    (repo / history.BASELINE / "manifest.json").unlink()
    with pytest.raises(ValueError, match="baseline manifest is missing"):
        history.snapshot_model_lineage(repo, tmp_path / "broken")


def test_bootstrap_preserves_low_level_failed_partial_attempts_and_no_credentials(tmp_path):
    root, staging, repo = tmp_path / "prep", tmp_path / "staging", tmp_path / "repo"
    write(root / "config.json", {"launch_config": {"provider": {"api_key": "SENTINEL_SECRET"}},
                                 "tools": {"aeneas": "/tool", "env": {"TOKEN": "SENTINEL_SECRET"}}})
    write(root / "work/extraction.json", {"crate": "crate"})
    write(staging / "config-template.json", {"provider": {"api_key": "SENTINEL_SECRET"}})
    write(staging / "extraction-config.json", {"source_tools": {"env": {"TOKEN": "SENTINEL_SECRET"}}})
    scratch = staging / "extraction/extract.one"
    write(scratch / "crate/lib.rs", "source")
    write(scratch / "gen/m.llbc", "partial llbc")
    write(scratch / "gen/M.lean", "partial Lean")
    write(scratch / "gen/M.olean", "compiled")
    history.record_extractor_attempt(staging / "extraction", scratch, metadata={"status": "failed", "steps": [],
                                      "extraction_config": {"source_tools": {"env": {"TOKEN": "SENTINEL_SECRET"}}}})
    history.record_bootstrap(root, staging, repo, inputs=history.capture_inputs(root), argv=["setup"], exit_code=1, tool_identities={})
    destination = tmp_path / "export"
    manifest = history.export_history(root, destination)
    refs = [r for r in manifest["records"] if r["kind"] == "extractor-attempt"]
    assert len(refs) == 1
    record, files = record_bytes(destination, refs[0])
    assert record["metadata"]["status"] == "failed"
    assert files["raw-generated/m.llbc"] == b"partial llbc"
    assert "raw-generated/M.olean" not in files
    assert not any(b"SENTINEL_SECRET" in p.read_bytes() for p in destination.rglob("*") if p.is_file())


def test_symlink_evidence_is_rejected(tmp_path):
    root = tmp_path / "prep"
    output, result = observation(root)
    (output / "Generated/link.lean").symlink_to(output / "Generated/M.lean")
    with pytest.raises(ValueError, match="symlink"):
        history.record_observation(root, output, result, inputs={})


def test_failed_extract_once_retains_raw_bytes_before_scratch_cleanup(tmp_path, monkeypatch):
    from scripts import extract_pv_model as extractor
    crate = tmp_path / "crate"
    write(crate / "Cargo.toml", '[package]\nname="sample"\nversion="0.1.0"\n')
    write(crate / "src/lib.rs", "pub fn f() {}\n")
    work = tmp_path / "work"
    work.mkdir()
    monkeypatch.setattr(extractor, "build_extraction_env", lambda *args: {})
    def failed_pipeline(crate_src, gen_out, **kwargs):
        write(gen_out / "m.llbc", "partially generated LLBC")
        write(gen_out / "M.lean", "partial Lean bytes")
        raise RuntimeError("test extraction failed")
    monkeypatch.setattr(extractor, "run_pipeline", failed_pipeline)
    with pytest.raises(RuntimeError, match="test extraction failed"):
        extractor.extract_once(crate, tmp_path / "out", charon_bin=Path("charon"),
                               aeneas_bin=Path("aeneas"), opam_switch="test",
                               cargo_target_dir=tmp_path / "target", work_root=work,
                               node_prefix="M", charon_version="v1", aeneas_version="v1", lean_version="v1",
                               extraction_config={"preserve_history": True})
    assert not list(work.glob("extract.*"))
    records = history._verified(work / "extraction-history")
    assert len(records) == 2
    completions = [reference for reference, record in records if record["kind"] == "extractor-attempt"]
    record, files = record_bytes(work / "extraction-history", completions[0])
    assert record["metadata"]["attempt_start_id"] in {reference["id"] for reference, _ in records}
    assert record["metadata"]["status"] == "failed"
    assert files["inputs/source/src/lib.rs"] == b"pub fn f() {}\n"
    assert files["raw-generated/m.llbc"] == b"partially generated LLBC"
    assert files["raw-generated/M.lean"] == b"partial Lean bytes"


def test_export_rechecks_bytes_at_copy_boundary(tmp_path, monkeypatch):
    root = tmp_path / "prep"
    output, result = observation(root)
    history.record_observation(root, output, result, inputs={})
    original = history._copy_record
    def corrupt_before_copy(source, destination, reference, record):
        blob = source / "blobs" / record["files"][0]["sha256"]
        blob.write_bytes(b"changed after verification")
        return original(source, destination, reference, record)
    monkeypatch.setattr(history, "_copy_record", corrupt_before_copy)
    with pytest.raises(ValueError, match="changed during export"):
        history.export_history(root, tmp_path / "export")


@pytest.mark.parametrize("kind", ["observation", "native-bootstrap", "extractor-attempt"])
def test_start_without_completion_never_claims_complete_history(tmp_path, kind):
    root = tmp_path / "prep"
    history.initialize_history(root)
    output = tmp_path / "output"
    metadata = {"output_path": str(output), "repo_path": str(tmp_path / "native")}
    start_id = history.start_attempt(root / history.HISTORY, kind,
                                     inputs={"inputs/source/lib.rs": b"original source"}, metadata=metadata)
    # Simulate a hard kill: no finally/completion callback was run.
    result = history.export_history(root, tmp_path / "export")
    assert result["history_complete"] is False
    assert result["coverage"] == "interrupted_or_inflight_attempts"
    assert result["interrupted_attempts"] == [{"start_id": start_id, "kind": kind,
        "status": "interrupted_or_inflight", "partial_artifacts": None, "artifact_coverage": "unavailable"}]
    start = next(ref for ref in result["records"] if ref["id"] == start_id)
    assert record_bytes(tmp_path / "export", start)[1]["inputs/source/lib.rs"] == b"original source"


def test_interrupted_bootstrap_recovers_child_start_and_partial_bytes_read_only(tmp_path):
    root, staging, repo = tmp_path / "prep", tmp_path / "staging", tmp_path / "native"
    history.initialize_history(root)
    history.start_attempt(root / history.HISTORY, "native-bootstrap", inputs={},
                          metadata={"output_path": str(staging), "repo_path": str(repo)})
    scratch = staging / "extraction/extract.interrupted"
    write(scratch / "gen/m.llbc", "unfinished llbc")
    child_id = history.start_attempt(staging / "extraction/extraction-history", "extractor-attempt", inputs={},
                                     metadata={"output_path": str(scratch)})
    before = {str(p): p.read_bytes() for directory in (root, staging) for p in directory.rglob("*") if p.is_file()}
    result = history.export_history(root, tmp_path / "export")
    after = {str(p): p.read_bytes() for directory in (root, staging) for p in directory.rglob("*") if p.is_file()}
    assert before == after
    assert not result["history_complete"]
    assert len(result["interrupted_attempts"]) == 2
    child = next(item for item in result["interrupted_attempts"] if item["start_id"] == child_id)
    assert record_bytes(tmp_path / "export", child["partial_artifacts"])[1]["raw-generated/m.llbc"] == b"unfinished llbc"


def test_observation_completion_closes_only_its_matching_start(tmp_path):
    root = tmp_path / "prep"
    history.initialize_history(root)
    output, result = observation(root)
    first = history.start_attempt(root / history.HISTORY, "observation", inputs={}, metadata={"output_path": str(output)})
    history.record_observation(root, output, result, inputs={}, attempt_start_id=first)
    assert history.export_history(root, tmp_path / "complete")["history_complete"]
    second = history.start_attempt(root / history.HISTORY, "observation", inputs={}, metadata={"output_path": str(output)})
    exported = history.export_history(root, tmp_path / "interrupted")
    assert not exported["history_complete"]
    assert [item["start_id"] for item in exported["interrupted_attempts"]] == [second]
    assert exported["interrupted_attempts"][0]["artifact_coverage"] == "recovered_partial"


def test_observation_start_precedes_tool_invocation_and_retains_input(tmp_path, monkeypatch):
    from trellis.modeling.observations import observe
    from trellis.modeling import environment
    root = tmp_path / "prep"
    history.initialize_history(root)
    write(root / "work/crate/lib.rs", "original source")
    write(root / "work/extraction.json", {"crate": "crate"})
    write(root / "config.json", {"tools": {}})
    def abrupt_stop(*args, **kwargs):
        assert any(record["kind"] == "attempt-start" for _, record in history._verified(root / history.HISTORY))
        raise SystemExit("simulated interruption before observation finally")
    monkeypatch.setattr(environment, "identify_tools", abrupt_stop)
    with pytest.raises(SystemExit, match="simulated interruption"):
        observe(root, tmp_path / "output")
    manifest = history.export_history(root, tmp_path / "export")
    assert manifest["coverage"] == "interrupted_or_inflight_attempts"
    start = next(ref for ref in manifest["records"] if ref["kind"] == "attempt-start")
    assert record_bytes(tmp_path / "export", start)[1]["inputs/source/lib.rs"] == b"original source"
