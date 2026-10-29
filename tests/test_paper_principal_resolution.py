"""Export resolves the same unique materialized principal as native admission."""
import json
from pathlib import Path
import shutil

import pytest

from trellis.paper_implementation import MANIFEST, export_paper_inputs, paper_input


@pytest.fixture(autouse=True)
def native_resolver(monkeypatch):
    binary = Path(__file__).resolve().parents[1] / "kernel/target/debug/trellis_runtime_cli"
    assert binary.is_file(), "Build the current runtime CLI before running these integration tests"
    monkeypatch.setenv("TRELLIS_TRELLIS_KERNEL_CMD", str(binary))


def subject(tmp_path, text, node="run"):
    bundle, prep, repo = (tmp_path / name for name in ("original", "preparation", "export"))
    bundle.mkdir()
    (bundle / "main.tex").write_text("The algorithm returns the input.\n")
    paper = paper_input(bundle, "main.tex", [{"id": "lines:1-1", "source_path": "main.tex",
        "start_line": 1, "end_line": 1, "selected_text": "The algorithm returns the input.", "excluded_clauses": []}],
        route="rust_aeneas", entry_points=[{"node_id": node, "declaration": node, "source_path": "crate/src/lib.rs"}],
        implementation_roots={"lines:1-1": [node]})
    shutil.copytree(bundle, prep / "paper")
    (prep / "config.json").write_text(json.dumps({"paper_input": paper}))
    (repo / "Tablet").mkdir(parents=True)
    (repo / f"Tablet/{node}.lean").write_text(text)
    (repo / "crate/src").mkdir(parents=True)
    (repo / "crate/src/lib.rs").write_text("pub fn run(n: u32) -> u32 { n }\n")
    (repo / "lean-toolchain").write_text("fixture\n")
    model = {"node": node, "lean": "def first_helper : Nat := 0", "namespace_context": "namespace Misleading",
             "provenance": {"source_file": "src/lib.rs"}}
    config = {"workflow": {}, "pv_tablet": {"extraction_models": [model]}}
    return prep, repo, config


def export(prep, repo, config):
    return export_paper_inputs(prep, repo, config, source_files=["crate/src/lib.rs"], build_files=["lean-toolchain"])


@pytest.mark.parametrize("text,node,principal", [
    ("def run_loop0.body : Nat := 0\ndef run_loop0 : Nat := 0\ndef run : Nat := 0\n", "run", "run"),
    ("namespace Crate\ndef run_loop0.body : Nat := 0\ndef run_loop0 : Nat := 0\ndef run_loop1.body : Nat := 0\ndef run_loop1 : Nat := 0\ndef run : Nat := 0\nend Crate\n", "run", "Crate.run"),
    ("namespace A\nsection\nnamespace Closed\ndef helper := 0\nend Closed\nend\nnamespace B\ndef run : Nat := 0\nend B\nend A\n", "run", "A.B.run"),
    ("namespace A\nnoncomputable section\ndef run_loop0 := 0\nend\n-- [TABLET NODE: run]\nnamespace B\ndef run : Nat := 0\nend B\nend A\n", "run", "A.B.run"),
    ("namespace A\ndef Tree.run : Nat := 0\nend A\n", "Tree_run", "A.Tree.run"),
    ("/- def run := 99 /- def run := 98 -/ -/\nnamespace A\n@[irreducible]\ndef\n run : Nat := 0\nend A\n", "run", "A.run"),
])
def test_export_uses_unique_principal_in_actual_namespace(tmp_path, text, node, principal):
    prep, repo, config = subject(tmp_path, text, node)
    before = (repo / f"Tablet/{node}.lean").read_bytes()
    export(prep, repo, config)
    manifest = json.loads((repo / MANIFEST).read_text())
    assert manifest["entry_points"][0]["declaration"] == principal
    assert (repo / f"Tablet/{node}.lean").read_bytes() == before
    assert config["pv_tablet"]["extraction_models"][0]["namespace_context"] == "namespace Misleading"


@pytest.mark.parametrize("text,reason", [
    ("def run_loop0.body := 0\ndef run_loop0 := 0\n", "no FILESPEC principal"),
    ("/- def run := 0 -/\ndef helper := 0\n", "no FILESPEC principal"),
    ("namespace A\ndef _root_.run : Nat := 0\nend A\n", "no FILESPEC principal"),
    ("def run := 0\ndef run := 1\n", "multiple FILESPEC principal"),
    ("namespace A\ndef run := 0\nend A\nnamespace B\ndef run := 0\nend B\n", "multiple FILESPEC principal"),
    ("end A\ndef run := 0\n", "unmatched `end`"),
])
def test_export_fails_closed_for_missing_or_ambiguous_principal(tmp_path, text, reason):
    prep, repo, config = subject(tmp_path, text)
    with pytest.raises(ValueError, match=reason):
        export(prep, repo, config)
    assert not (repo / MANIFEST).exists()
    assert "paper_implementation" not in config


def test_export_requires_unique_model_and_materialized_node(tmp_path):
    prep, repo, config = subject(tmp_path, "def run := 0\n")
    models = config["pv_tablet"]["extraction_models"]
    models.append(dict(models[0]))
    with pytest.raises(ValueError, match="multiple extracted models"):
        export(prep, repo, config)
    models.pop()
    (repo / "Tablet/run.lean").unlink()
    with pytest.raises(ValueError, match="Missing|missing"):
        export(prep, repo, config)
    assert not (repo / MANIFEST).exists()
