from __future__ import annotations

import hashlib
import json
import shutil
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "import_challenge_targets.py"
FIXTURE = ROOT / "tests" / "fixtures" / "lean_eval_unit_distance"


def run_importer(
    problem_dir: Path, out: Path | None = None
) -> subprocess.CompletedProcess[str]:
    cmd = [sys.executable, str(SCRIPT), str(problem_dir)]
    if out is not None:
        cmd += ["--out", str(out)]
    return subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)


def import_fixture(tmp_path: Path) -> dict:
    out = tmp_path / "challenge_targets.json"
    result = run_importer(FIXTURE, out)
    assert result.returncode == 0, result.stderr
    return json.loads(out.read_text(encoding="utf-8"))


def targets_by_name(spec: dict) -> dict[str, dict]:
    return {t["name"]: t for t in spec["targets"]}


def test_target_names_kinds_and_namespace_contexts(tmp_path: Path) -> None:
    spec = import_fixture(tmp_path)

    assert spec["schema_version"] == 1
    assert spec["problem_id"] == "unit_distance_upper_bound"
    assert spec["toolchain"] == {
        "MATHLIB_TOOLCHAIN": "leanprover/lean4:v4.30.0-rc2",
        "MATHLIB_REV": "5450b53e5ddc",
    }

    assert [(t["name"], t["kind"]) for t in spec["targets"]] == [
        ("unit_distance_upper_bound", "theorem"),
        ("planeDim", "def"),
        ("unitDist", "def"),
    ]

    by_name = targets_by_name(spec)
    assert by_name["unit_distance_upper_bound"]["namespace_context"] == ""
    assert by_name["planeDim"]["namespace_context"] == "LeanEval.Combinatorics"
    assert by_name["unitDist"]["namespace_context"] == "LeanEval.Combinatorics"
    assert by_name["unit_distance_upper_bound"]["id"] == (
        "challenge:unit_distance_upper_bound"
    )
    assert by_name["unit_distance_upper_bound"]["informal"].startswith(
        "# unit_distance_upper_bound"
    )
    assert by_name["planeDim"]["informal"] == ""


def test_theorem_proof_is_stripped_at_filespec_boundary(tmp_path: Path) -> None:
    spec = import_fixture(tmp_path)
    lean = targets_by_name(spec)["unit_distance_upper_bound"]["lean"]

    assert lean.startswith("theorem unit_distance_upper_bound :")
    last_line = lean.split("\n")[-1]
    assert last_line.endswith(":=") or last_line.endswith("by")
    assert "sorry" not in lean
    assert not lean.endswith("\n")
    # The multi-line statement survives intact up to the boundary.
    assert "∀ P : Finset (EuclideanSpace ℝ (Fin 2))," in lean


def test_single_line_def_is_rebroken_at_assign(tmp_path: Path) -> None:
    spec = import_fixture(tmp_path)
    by_name = targets_by_name(spec)

    assert by_name["planeDim"]["lean"] == "def planeDim : Nat :=\n  2"

    # The already-conformant multi-line def is unchanged: its first line ends
    # at ':=' and the value follows, with namespace wrappers removed.
    unit_dist = by_name["unitDist"]["lean"]
    lines = unit_dist.split("\n")
    assert lines[0] == (
        "noncomputable def unitDist "
        "(P : Finset (EuclideanSpace ℝ (Fin 2))) : ℕ :="
    )
    assert lines[1].startswith("  ")
    assert "namespace" not in unit_dist


def test_rerun_is_byte_identical(tmp_path: Path) -> None:
    out_a = tmp_path / "a.json"
    out_b = tmp_path / "b.json"
    assert run_importer(FIXTURE, out_a).returncode == 0
    assert run_importer(FIXTURE, out_b).returncode == 0
    assert out_a.read_bytes() == out_b.read_bytes()


def test_source_hashes_match_fixture_files(tmp_path: Path) -> None:
    spec = import_fixture(tmp_path)

    expected = {
        name: hashlib.sha256((FIXTURE / name).read_bytes()).hexdigest()
        for name in [
            "Challenge.lean",
            "ChallengeDeps.lean",
            "README.md",
            "config.json",
            "lakefile.toml",
            "lean-toolchain",
        ]
    }
    assert spec["source_hashes"] == expected
    assert list(spec["source_hashes"]) == sorted(expected)

    for target in spec["targets"]:
        provenance = target["provenance"]
        assert provenance["problem_id"] == "unit_distance_upper_bound"
        assert provenance["source_sha256"] == expected[provenance["source_file"]]


def test_theorem_name_mismatch_exits_nonzero(tmp_path: Path) -> None:
    problem_dir = tmp_path / "problem"
    shutil.copytree(FIXTURE, problem_dir)
    config_path = problem_dir / "config.json"
    config = json.loads(config_path.read_text(encoding="utf-8"))
    config["theorem_names"] = ["some_other_theorem"]
    config_path.write_text(json.dumps(config), encoding="utf-8")

    result = run_importer(problem_dir, tmp_path / "out.json")
    assert result.returncode != 0
    assert "theorem name mismatch" in result.stderr
