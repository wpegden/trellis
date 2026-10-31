"""Paper Rust packaging removes only the unused legacy assumption scaffold."""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from scripts.extract_pv_model import (
    _empty_assumptions_lean,
    _empty_assumptions_tex,
    split_generated_lean,
)
from trellis.modeling.native import materialize_model_layout
from trellis.pv_model_support import empty_model_support, sync_model_support


def _json(path: Path, value) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value) + "\n")


def _fixture(repo: Path, *, required_v1: bool = False) -> str:
    raw = ("import Aeneas\nnamespace example\n"
           "/-- [example::identity]: Source: 'src/lib.rs', lines 1:0-1:39 -/\n"
           "def identity (x : Nat) : Nat := x\nend example\n")
    (repo / "generated.lean").write_text(raw)
    split = split_generated_lean(raw, expose_types=True)
    config = {"pv_tablet": {"extraction_models": [{
        "id": "model:" + node.name, "node": node.name, "name": node.name,
        "lean": node.region, "namespace_context": "namespace example",
        "provenance": {"source_file": "src/lib.rs", "source_sha256": "a" * 64},
    } for node in split.nodes]}}
    if required_v1:
        config["trust_base"] = {"mode": "required_v1"}
    _json(repo / "trellis.config.json", config)
    _json(repo / "modeling-input/manifest.json", {})
    tablet = repo / "Tablet"
    tablet.mkdir()
    (tablet / "Preamble.lean").write_text(split.preamble_text)
    (tablet / "Preamble.tex").write_text("")
    (tablet / "Assumptions.lean").write_text(_empty_assumptions_lean())
    (tablet / "Assumptions.tex").write_text(_empty_assumptions_tex())
    for node in split.nodes:
        (tablet / f"{node.name}.lean").write_text(node.text)
        (tablet / f"{node.name}.tex").write_text("")
    _json(tablet / "model-support.json", empty_model_support())
    return raw


def _admit_paper_config(repo: Path) -> None:
    path = repo / "trellis.config.json"
    config = json.loads(path.read_text())
    config["paper_implementation"] = {
        "version": 1, "route": "rust_aeneas", "manifest": "paper-implementation/manifest.json",
    }
    _json(path, config)


def test_paper_legacy_layout_removes_exact_empty_scaffolds_and_survives_later_sync(tmp_path):
    raw = _fixture(tmp_path)
    before = json.loads((tmp_path / "trellis.config.json").read_text())["pv_tablet"]["extraction_models"]
    materialize_model_layout(tmp_path, paper_implementation=True)
    assert not (tmp_path / "Tablet/Assumptions.lean").exists()
    assert not (tmp_path / "Tablet/Assumptions.tex").exists()
    assert "import Tablet.Assumptions" not in (tmp_path / "Tablet/identity.lean").read_text()
    assert (tmp_path / "generated.lean").read_text() == raw
    after = json.loads((tmp_path / "trellis.config.json").read_text())["pv_tablet"]["extraction_models"]
    assert [(m["lean"], m["provenance"]) for m in after] == [(m["lean"], m["provenance"]) for m in before]
    # Thereafter no privileged preparation flag: use the actual mode config.
    _admit_paper_config(tmp_path)
    assert sync_model_support(tmp_path)["matches"]
    assert not sync_model_support(tmp_path, write=True)["changed"]


@pytest.mark.parametrize("name,change", [
    ("Assumptions.lean", "axiom fabricated : False\n"),
    ("Assumptions.lean", "def validity (x : Nat) := x > 0\n"),
    ("Assumptions.tex", "\\begin{definition}Additional premise.\\end{definition}\n"),
    ("Assumptions.tex", "\n"),
])
def test_paper_layout_rejects_any_noncanonical_scaffold_without_changing_artifacts(tmp_path, name, change):
    _fixture(tmp_path)
    path = tmp_path / "Tablet" / name
    path.write_text(path.read_text() + change)
    before = {p.relative_to(tmp_path): p.read_bytes() for p in tmp_path.rglob("*") if p.is_file()}
    with pytest.raises(ValueError, match="nonempty or altered assumptions"):
        materialize_model_layout(tmp_path, paper_implementation=True)
    assert {p.relative_to(tmp_path): p.read_bytes() for p in tmp_path.rglob("*") if p.is_file()} == before


def test_paper_layout_rejects_partial_scaffold_pair(tmp_path):
    _fixture(tmp_path)
    (tmp_path / "Tablet/Assumptions.tex").unlink()
    with pytest.raises(ValueError, match="complete exact-empty"):
        materialize_model_layout(tmp_path, paper_implementation=True)
    assert (tmp_path / "Tablet/Assumptions.lean").is_file()


@pytest.mark.parametrize("paper,required_v1", [(False, False), (False, True), (True, True)])
def test_ordinary_pv_and_required_v1_keep_existing_support_unchanged(tmp_path, paper, required_v1):
    _fixture(tmp_path, required_v1=required_v1)
    path = tmp_path / "Tablet/Assumptions.lean"
    support = path.read_text() + "def authenticatedSeedSupport : Nat := 0\n"
    path.write_text(support)
    materialize_model_layout(tmp_path, paper_implementation=paper)
    assert path.read_text() == support
    assert (tmp_path / "Tablet/Assumptions.tex").read_text() == _empty_assumptions_tex()
    assert "import Tablet.Assumptions" in (tmp_path / "Tablet/identity.lean").read_text()
    if paper:
        _admit_paper_config(tmp_path)
    assert sync_model_support(tmp_path)["matches"]


def test_later_paper_sync_rejects_reintroduced_scaffold(tmp_path):
    _fixture(tmp_path)
    materialize_model_layout(tmp_path, paper_implementation=True)
    _admit_paper_config(tmp_path)
    (tmp_path / "Tablet/Assumptions.lean").write_text(_empty_assumptions_lean())
    with pytest.raises(ValueError, match="unexpected assumptions support"):
        sync_model_support(tmp_path, write=True)
    assert (tmp_path / "Tablet/Assumptions.lean").is_file()
    assert "import Tablet.Assumptions" not in (tmp_path / "Tablet/identity.lean").read_text()
