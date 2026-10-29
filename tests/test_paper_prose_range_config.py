"""Paper source-range routing; no agent, compiler, or provider activity."""
import json
from pathlib import Path
import shutil

import pytest

from trellis import config
from trellis.paper_implementation import (
    lean_lakefile, lean_lake_manifest, paper_input, workflow_targets, write_manifest,
)


def dump(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value))


def paper_fixture(tmp_path, route):
    bundle = tmp_path / "original"
    bundle.mkdir()
    (bundle / "main.tex").write_bytes(b"Original introduction.\r\nThe algorithm returns the input.\r\n")
    paper = paper_input(bundle, "main.tex", [{"id": "lines:2-2", "source_path": "main.tex",
        "start_line": 2, "end_line": 2, "selected_text": "The algorithm returns the input.", "excluded_clauses": []}],
        route=route, entry_points=[{"node_id": "Algorithm", "declaration": "Algorithm",
            "source_path": "Tablet/Algorithm.lean" if route == "lean_native" else "crate/src/lib.rs"}],
        implementation_roots={"lines:2-2": ["Algorithm"]})
    repo = tmp_path / "repo"
    repo.mkdir(); shutil.copytree(bundle, repo / "paper")
    raw = {"repo_path": str(repo), "worker": {"provider": "codex"}, "reviewer": {"provider": "codex"},
           "tmux": {}, "workflow": {"paper_tex_path": paper["paper_path"], "main_result_targets": workflow_targets(paper)}}
    return repo, paper, raw


def resolver_spy(monkeypatch):
    calls = []
    def resolve(payload):
        calls.append(payload)
        target = {"start_line": 2, "end_line": 2}
        return {"status": "resolve_main_result_targets_ok", "output": {"targets": [target], "available_labels": [],
            "preview": [{"target": target, "env": "prose", "text": "The algorithm returns the input.\r\n", "start_line": 2, "end_line": 2}]}}
    monkeypatch.setattr(config, "run_kernel_cli", resolve)
    return calls


def test_resolver_wrapper_default_does_not_enable_prose(tmp_path, monkeypatch):
    calls = resolver_spy(monkeypatch)
    config.resolve_main_result_targets_via_kernel(paper_path=tmp_path / "original.tex")
    assert "allow_prose_ranges" not in calls[-1]
    result = config.resolve_main_result_targets_via_kernel(paper_path=tmp_path / "original.tex", allow_prose_ranges=True)
    assert calls[-1]["allow_prose_ranges"] is True
    assert result["preview"][0]["text"] == "The algorithm returns the input.\r\n"


@pytest.mark.parametrize("route,protocol", [("rust_aeneas", "native-v1"), ("lean_native", "paper-lean-preparation/v1")])
def test_preparation_config_load_derives_ranges_from_existing_pinned_paper(tmp_path, monkeypatch, route, protocol):
    repo, paper, raw = paper_fixture(tmp_path, route)
    dump(repo / "config.json", {"paper_input": paper})
    dump(repo / ".trellis/modeling/state.json", {"verification_protocol": protocol})
    path = repo / "trellis.config.json"; dump(path, raw)
    calls = resolver_spy(monkeypatch)
    loaded = config.load_config(path)
    assert calls[-1]["allow_prose_ranges"] is True
    assert calls[-1]["raw_targets"] == [{"start_line": 2, "end_line": 2}]
    assert loaded.workflow.main_result_targets == [{"start_line": 2, "end_line": 2}]
    assert loaded.paper_implementation is None, "preparation must not pretend to be an extracted/native subject"
    (repo / "paper/main.tex").write_text("Changed original paper")
    with pytest.raises(config.ConfigError, match="Original paper input changed"):
        config.load_config(path)
    assert len(calls) == 1, "changed original paper must fail before resolver dispatch"


@pytest.mark.parametrize("change", ["route", "paper_path", "targets"])
def test_preparation_identity_mismatch_does_not_enable_ranges(tmp_path, change):
    repo, paper, raw = paper_fixture(tmp_path, "rust_aeneas")
    dump(repo / "config.json", {"paper_input": paper})
    dump(repo / ".trellis/modeling/state.json", {"verification_protocol": "native-v1"})
    if change == "route":
        paper["route"] = "lean_native"; dump(repo / "config.json", {"paper_input": paper})
    if change == "paper_path": raw["workflow"]["paper_tex_path"] = "reference.tex"
    if change == "targets": raw["workflow"]["main_result_targets"][0]["start_line"] = 1
    with pytest.raises(config.ConfigError, match="original paper target locators"):
        config._paper_implementation_prose_ranges(repo, raw)


def test_ordinary_repo_config_or_unrelated_state_does_not_enable_ranges(tmp_path):
    repo, _, raw = paper_fixture(tmp_path, "lean_native")
    (repo / "config.json").write_text("unrelated non-JSON config")
    assert not config._paper_implementation_prose_ranges(repo, raw)
    dump(repo / ".trellis/modeling/state.json", {"verification_protocol": "another-tool/v1"})
    assert not config._paper_implementation_prose_ranges(repo, raw)
    dump(repo / ".trellis/modeling/state.json", {"verification_protocol": "native-v1"})
    dump(repo / "config.json", {"goals": []})
    assert not config._paper_implementation_prose_ranges(repo, raw)


def native_paper_fixture(tmp_path):
    repo, paper, raw = paper_fixture(tmp_path, "lean_native")
    (repo / "Tablet").mkdir()
    (repo / "Tablet/Algorithm.lean").write_text("-- [TABLET NODE: Algorithm]\ndef Algorithm (n : Nat) : Nat := n\n")
    (repo / "lean-toolchain").write_text("leanprover/lean4:v4.30.0-rc1\n")
    (repo / "lakefile.toml").write_text(lean_lakefile(["Algorithm"]))
    (repo / "lake-manifest.json").write_text(lean_lake_manifest())
    raw["paper_implementation"] = write_manifest(repo, paper=paper, source_files=["Tablet/Algorithm.lean"],
        build_files=["lean-toolchain", "lakefile.toml", "lake-manifest.json"])
    path = repo / "trellis.config.json"; dump(path, raw)
    return repo, path


def test_native_paper_config_keeps_manifest_and_source_range_authority(tmp_path, monkeypatch):
    repo, path = native_paper_fixture(tmp_path)
    calls = resolver_spy(monkeypatch)
    loaded = config.load_config(path)
    assert calls[-1]["allow_prose_ranges"] is True
    assert loaded.paper_implementation["manifest"]["targets"][0]["id"] == "lines:2-2"
    assert loaded.workflow.paper_tex_path == repo / "paper/main.tex"
    assert (repo / "paper/main.tex").read_bytes().endswith(b"The algorithm returns the input.\r\n")


def test_native_prose_config_resolves_through_actual_kernel_cli(tmp_path, monkeypatch):
    binary = Path(__file__).resolve().parents[1] / "kernel/target/debug/trellis_runtime_cli"
    assert binary.is_file(), "build the current runtime CLI before this bounded integration test"
    monkeypatch.setenv("TRELLIS_TRELLIS_KERNEL_CMD", str(binary))
    repo, path = native_paper_fixture(tmp_path)
    loaded = config.load_config(path)
    assert loaded.workflow.main_result_targets == [{"start_line": 2, "end_line": 2}]
    assert loaded.paper_implementation["manifest"]["targets"][0]["id"] == "lines:2-2"
    resolved = config.resolve_main_result_targets_via_kernel(paper_path=repo / "paper/main.tex",
        raw_targets=loaded.workflow.main_result_targets, allow_prose_ranges=True)
    assert resolved["preview"][0]["text"] == "The algorithm returns the input.\r\n"
    assert resolved["preview"][0]["env"] == "prose"
    with pytest.raises(config.ConfigError, match="Could not locate paper text"):
        config.resolve_main_result_targets_via_kernel(paper_path=repo / "paper/main.tex",
            raw_targets=loaded.workflow.main_result_targets)
    dump(tmp_path / "actual-kernel-prose-resolution.json", {"config_path": str(path), "resolved": resolved,
        "original_target": loaded.paper_implementation["manifest"]["targets"][0]})
