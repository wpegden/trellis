"""Synthetic export fixture consumed by the real Rust admission test.

This exercises packaging and admission, not Rust extraction or a proof claim.
"""
import json
from pathlib import Path
import shutil
import sys

from scripts.extract_pv_model import crate_source_sha256, split_generated_lean
from trellis.paper_implementation import export_paper_inputs, paper_input


def create(root: Path, loops: int):
    original, prep, repo = (root / name for name in ("original", "preparation", "export"))
    original.mkdir(parents=True)
    (original / "main.tex").write_text("The algorithm returns its input.\n")
    paper = paper_input(original, "main.tex", [{"id": "lines:1-1", "source_path": "main.tex",
        "start_line": 1, "end_line": 1, "selected_text": "The algorithm returns its input.", "excluded_clauses": []}],
        route="rust_aeneas", entry_points=[{"node_id": "run", "declaration": "run", "source_path": "crate/src/lib.rs"}],
        implementation_roots={"lines:1-1": ["run"]})
    shutil.copytree(original, prep / "paper")
    (prep / "config.json").write_text(json.dumps({"paper_input": paper}))
    (repo / "crate/src").mkdir(parents=True)
    (repo / "crate/src/lib.rs").write_text("pub fn run(n: u32) -> u32 { n }\n")
    (repo / "lean-toolchain").write_text("leanprover/lean4:v4.30.0-rc2\n")
    raw = "import Aeneas\nnamespace fixture\n"
    for i in range(loops):
        for suffix in (f"run_loop{i}.body", f"run_loop{i}"):
            attribute = "rust_loop_body" if suffix.endswith(".body") else "rust_loop"
            raw += f"/-- [fixture::run]: loop {i}: Source: 'src/lib.rs', lines 1:0-1:40 -/\n@[{attribute}]\ndef {suffix} (n : Nat) : Nat := n\n\n"
    raw += "/-- [fixture::run]: Source: 'src/lib.rs', lines 1:0-1:40 -/\ndef run (n : Nat) : Nat := n\nend fixture\n"
    (repo / "generated.lean").write_text(raw)
    split = split_generated_lean(raw, expose_types=True)
    assert [node.name for node in split.nodes] == ["run"]
    (repo / "Tablet").mkdir()
    node = split.nodes[0]
    (repo / "Tablet/run.lean").write_text(node.text)
    model = {"id": "model:run", "node": "run", "name": "run", "kind": "def", "informal": "",
        "lean": node.region, "namespace_context": "namespace fixture",
        "provenance": {"problem_id": "fixture", "source_file": "src/lib.rs",
                       "source_sha256": crate_source_sha256(repo / "crate")}}
    config = {"workflow": {}, "modeling": {"enabled": True, "preservation": True},
              "pv_tablet": {"extraction_models": [model]}}
    export_paper_inputs(prep, repo, config, source_files=["crate/src/lib.rs", "generated.lean"], build_files=["lean-toolchain"])
    (repo / "trellis.config.json").write_text(json.dumps(config))


if __name__ == "__main__":
    create(Path(sys.argv[1]), int(sys.argv[2]))
