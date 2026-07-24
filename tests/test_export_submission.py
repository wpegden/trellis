from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "export_submission.py"

PROBLEM_ID = "demo_problem"

FOO_THM_LEAN = "theorem foo_thm : bar_def = 1 := by"
BAR_DEF_LEAN = "def bar_def : Nat :=\n  1"


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _write_run_repo(root: Path) -> Path:
    run_repo = root / "run"
    tablet = run_repo / "Tablet"
    tablet.mkdir(parents=True)
    (run_repo / "Tablet.lean").write_text(
        "import Tablet.bar_def\n"
        "import Tablet.helper_lemma\n"
        "import Tablet.foo_thm\n"
        "import Tablet.Axioms\n",
        encoding="utf-8",
    )
    (tablet / "Axioms.lean").write_text(
        "-- kernel-managed axiom ledger\n", encoding="utf-8"
    )
    (tablet / "bar_def.lean").write_text(
        "-- [TABLET NODE: bar_def]\n"
        "def bar_def : Nat :=\n"
        "-- BODY\n"
        "  1\n",
        encoding="utf-8",
    )
    (tablet / "helper_lemma.lean").write_text(
        "import Tablet.bar_def\n"
        "\n"
        "-- [TABLET NODE: helper_lemma]\n"
        "theorem helper_lemma : bar_def = 1 := by\n"
        "-- BODY\n"
        "  rfl\n",
        encoding="utf-8",
    )
    (tablet / "foo_thm.lean").write_text(
        "import Tablet.bar_def\n"
        "import Tablet.helper_lemma\n"
        "open Chal\n"
        "\n"
        "-- [TABLET NODE: foo_thm]\n"
        f"{FOO_THM_LEAN}\n"
        "-- BODY\n"
        "  simpa using helper_lemma\n",
        encoding="utf-8",
    )
    return run_repo


def _write_download(root: Path) -> Path:
    download = root / "download"
    download.mkdir()
    (download / "Challenge.lean").write_text(
        "import ChallengeDeps\n"
        "\n"
        "namespace Chal\n"
        "\n"
        f"{FOO_THM_LEAN}\n"
        "  sorry\n"
        "\n"
        "end Chal\n",
        encoding="utf-8",
    )
    (download / "ChallengeDeps.lean").write_text(
        "namespace Chal\n"
        "\n"
        "def bar_def : Nat :=\n"
        "  1\n"
        "\n"
        "end Chal\n",
        encoding="utf-8",
    )
    (download / "config.json").write_text(
        json.dumps({"theorem_names": ["foo_thm"]}) + "\n", encoding="utf-8"
    )
    (download / "lean-toolchain").write_text(
        "leanprover/lean4:v4.30.0-rc2\n", encoding="utf-8"
    )
    (download / "lakefile.toml").write_text(
        'name = "challenge_template"\n'
        'version = "0.1.0"\n'
        "\n"
        "[[lean_lib]]\n"
        'name = "Challenge"\n'
        "\n"
        "[[lean_lib]]\n"
        'name = "ChallengeDeps"\n',
        encoding="utf-8",
    )
    return download


def _write_spec(root: Path, download: Path) -> Path:
    source_files = [
        "Challenge.lean",
        "ChallengeDeps.lean",
        "config.json",
        "lean-toolchain",
        "lakefile.toml",
    ]
    spec = {
        "schema_version": 1,
        "problem_id": PROBLEM_ID,
        "toolchain": {
            "MATHLIB_TOOLCHAIN": "leanprover/lean4:v4.30.0-rc2",
            "MATHLIB_REV": "5450b53e5ddc",
        },
        "source_hashes": {name: _sha256(download / name) for name in source_files},
        "targets": [
            {
                "id": "challenge:bar_def",
                "kind": "def",
                "name": "bar_def",
                "lean": BAR_DEF_LEAN,
                "namespace_context": "Chal",
                "informal": "bar_def is one.",
                "provenance": {
                    "problem_id": PROBLEM_ID,
                    "source_file": "ChallengeDeps.lean",
                    "source_sha256": _sha256(download / "ChallengeDeps.lean"),
                },
            },
            {
                "id": "challenge:foo_thm",
                "kind": "theorem",
                "name": "foo_thm",
                "lean": FOO_THM_LEAN,
                "namespace_context": "Chal",
                "informal": "bar_def equals one.",
                "provenance": {
                    "problem_id": PROBLEM_ID,
                    "source_file": "Challenge.lean",
                    "source_sha256": _sha256(download / "Challenge.lean"),
                },
            },
        ],
    }
    spec_path = root / "challenge_targets.json"
    spec_path.write_text(json.dumps(spec, indent=2) + "\n", encoding="utf-8")
    return spec_path


def _build_fixture(tmp_path: Path) -> tuple[Path, Path, Path, Path]:
    run_repo = _write_run_repo(tmp_path)
    download = _write_download(tmp_path)
    spec_path = _write_spec(tmp_path, download)
    out = tmp_path / "submission"
    return run_repo, spec_path, download, out


def _run_export(
    run_repo: Path,
    spec_path: Path,
    download: Path,
    out: Path,
    *extra: str,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            str(run_repo),
            str(spec_path),
            str(download),
            "--out",
            str(out),
            "--skip-lake",
            *extra,
        ],
        capture_output=True,
        text=True,
    )


def _tree_bytes(root: Path) -> dict[str, bytes]:
    return {
        str(p.relative_to(root)): p.read_bytes()
        for p in sorted(root.rglob("*"), key=str)
        if p.is_file()
    }


def test_provenance_mismatch_exits_nonzero(tmp_path: Path) -> None:
    run_repo, spec_path, download, out = _build_fixture(tmp_path)
    challenge = download / "Challenge.lean"
    challenge.write_text(
        challenge.read_text(encoding="utf-8") + "-- tampered\n", encoding="utf-8"
    )

    proc = _run_export(run_repo, spec_path, download, out)

    assert proc.returncode != 0
    assert "Challenge.lean" in proc.stderr
    assert "mismatch" in proc.stderr
    assert not (out / "Submission.lean").exists()


def test_provenance_missing_file_exits_nonzero(tmp_path: Path) -> None:
    run_repo, spec_path, download, out = _build_fixture(tmp_path)
    (download / "config.json").unlink()

    proc = _run_export(run_repo, spec_path, download, out)

    assert proc.returncode != 0
    assert "config.json" in proc.stderr


def test_submission_tree_mirrors_nodes_with_rewritten_imports(tmp_path: Path) -> None:
    run_repo, spec_path, download, out = _build_fixture(tmp_path)

    proc = _run_export(run_repo, spec_path, download, out)

    assert proc.returncode == 0, proc.stderr
    submission = out / "Submission"
    names = sorted(p.name for p in submission.rglob("*.lean"))
    assert names == ["bar_def.lean", "foo_thm.lean", "helper_lemma.lean"]

    foo = (submission / "foo_thm.lean").read_text(encoding="utf-8")
    assert "import Submission.bar_def" in foo
    assert "import Submission.helper_lemma" in foo
    assert "import Tablet." not in foo
    assert "-- [TABLET NODE: foo_thm]" in foo
    assert "-- BODY" in foo


def test_submission_root_bridges_theorem_targets(tmp_path: Path) -> None:
    run_repo, spec_path, download, out = _build_fixture(tmp_path)

    proc = _run_export(run_repo, spec_path, download, out)

    assert proc.returncode == 0, proc.stderr
    root_text = (out / "Submission.lean").read_text(encoding="utf-8")
    assert "import ChallengeDeps" in root_text
    assert "import Submission.foo_thm" in root_text
    assert "namespace Submission" in root_text
    assert "end Submission" in root_text
    assert "open Chal in" in root_text
    assert (
        "theorem foo_thm : bar_def = 1 := by\n  exact _root_.foo_thm" in root_text
    )
    # def targets ship in Submission/ only; the bridge declares theorems.
    assert "def bar_def" not in root_text


def test_lakefile_name_is_problem_id(tmp_path: Path) -> None:
    run_repo, spec_path, download, out = _build_fixture(tmp_path)

    proc = _run_export(run_repo, spec_path, download, out)

    assert proc.returncode == 0, proc.stderr
    lakefile = (out / "lakefile.toml").read_text(encoding="utf-8")
    assert f'name = "{PROBLEM_ID}"' in lakefile
    assert 'name = "challenge_template"' not in lakefile
    # The rest of the download's lakefile is preserved.
    assert 'name = "Challenge"' in lakefile
    assert 'version = "0.1.0"' in lakefile


def test_overlay_places_only_submission_files_on_pristine_copy(
    tmp_path: Path,
) -> None:
    run_repo, spec_path, download, out = _build_fixture(tmp_path)
    pristine = _tree_bytes(download)

    proc = _run_export(run_repo, spec_path, download, out)

    assert proc.returncode == 0, proc.stderr
    overlay = out.parent / f"{out.name}-overlay"
    assert overlay.is_dir()

    overlay_files = set(_tree_bytes(overlay))
    expected = set(pristine) | {
        "Submission.lean",
        "Submission/bar_def.lean",
        "Submission/foo_thm.lean",
        "Submission/helper_lemma.lean",
    }
    assert overlay_files == expected

    # Pristine download files are untouched: the submission lakefile.toml
    # in particular must NOT be copied over the problem's.
    overlay_bytes = _tree_bytes(overlay)
    for rel, data in pristine.items():
        assert overlay_bytes[rel] == data
    assert overlay_bytes["Submission.lean"] == (out / "Submission.lean").read_bytes()


def test_success_prints_prefilled_issue_url(tmp_path: Path) -> None:
    run_repo, spec_path, download, out = _build_fixture(tmp_path)

    proc = _run_export(
        run_repo, spec_path, download, out, "--model-id", "trellis/test-model"
    )

    assert proc.returncode == 0, proc.stderr
    url_lines = [
        line
        for line in proc.stdout.splitlines()
        if line.startswith("https://github.com/leanprover/lean-eval/issues/new?")
    ]
    assert len(url_lines) == 1
    url = url_lines[0]
    assert "title=" in url and "body=" in url
    assert PROBLEM_ID in url
    assert "trellis%2Ftest-model" in url
    assert "%3CSUBMISSION_REPO_URL%3E" in url


def test_rerun_is_byte_identical(tmp_path: Path) -> None:
    run_repo, spec_path, download, out = _build_fixture(tmp_path)

    first = _run_export(run_repo, spec_path, download, out)
    assert first.returncode == 0, first.stderr
    first_out = _tree_bytes(out)
    overlay = out.parent / f"{out.name}-overlay"
    first_overlay = _tree_bytes(overlay)

    second = _run_export(run_repo, spec_path, download, out)
    assert second.returncode == 0, second.stderr

    assert _tree_bytes(out) == first_out
    assert _tree_bytes(overlay) == first_overlay
