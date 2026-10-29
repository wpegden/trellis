"""Preparation compatibility must remain diagnostic and preserve closure policy."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

import pytest

from trellis.modeling.axioms import (CANONICAL_AXIOMS, PROBE, SCHEMA, compatibility,
                                    observe_module, probe_identity, read_report)
from trellis.modeling.cache import Cache
from trellis.modeling.native import NativeExportCandidateError, validate_export


def test_canonical_policy_matches_kernel():
    text = (PROBE.parent.parent / "kernel/src/model.rs").read_text()
    floor = re.search(r"pub const CANONICAL_APPROVED_AXIOMS:.*?&\[(.*?)\];", text, re.S)[1]
    assert set(CANONICAL_AXIOMS) == set(re.findall(r'"([^"]+)"', floor))


@pytest.fixture
def lean(tmp_path):
    binary = Path(os.environ.get("TRELLIS_TEST_LEAN", str(
        Path.home() / ".elan/toolchains/leanprover--lean4---v4.30.0-rc2/bin/lean")))
    if not binary.is_file():
        pytest.skip("Set TRELLIS_TEST_LEAN to an installed supported Lean binary")
    env = {**os.environ, "PATH": str(binary.parent) + os.pathsep + os.environ["PATH"],
           "LEAN_PATH": str(tmp_path)}
    return str(binary), env


def inspect(tmp_path, lean, text):
    binary, env = lean
    source = tmp_path / "M.lean"
    source.write_text(text)
    output = tmp_path / "observation"
    output.mkdir(exist_ok=True)
    summary, step = observe_module(source, "M", output=output, tools={"lean": binary}, env=env,
                                   environment="test-resources", dependencies={}, cache=Cache(tmp_path / "cache"))
    assert step["exit_code"] == 0, Path(step["log"]).read_text()
    assert not summary.get("error"), summary
    return source, summary, read_report(Path(summary["report"]), "M")


def observation(source, summary):
    return {"raw_models": {"M": str(source)}, "modules": {"M": str(source)},
            "environment": "test-resources", "axiom_dependencies": {
                "schema": SCHEMA, "probe_sha256": probe_identity(), "environment": "test-resources",
                "raw_sha256": {"M": hashlib.sha256(source.read_bytes()).hexdigest()},
                "modules": {"M": summary}}}


def test_deep_source_supported_by_frontend_has_complete_axiom_inventory(tmp_path, lean):
    # The old --run-main frontend overflowed its process stack on this ordinary
    # do block even though the normal Lean frontend elaborated it successfully.
    binary, env = lean
    text = ('set_option maxRecDepth 65536\nset_option maxHeartbeats 0\n'
            'def deep (n : Nat) : Option Nat := do\n'
            + ''.join(f'  let n ← some (n + {i})\n' for i in range(1500))
            + '  return n\ndef afterDeep := deep 0\n')
    source = tmp_path / "M.lean"
    source.write_text(text)
    ordinary = subprocess.run([binary, str(source)], cwd=tmp_path, env=env,
                              capture_output=True, text=True, timeout=120)
    assert ordinary.returncode == 0, ordinary.stdout + ordinary.stderr
    _, summary, report = inspect(tmp_path, lean, text)
    assert summary["complete"] and report["complete"]
    rows = {row["name"]: row for row in report["declarations"]}
    assert {"deep", "afterDeep"} <= rows.keys()
    assert all(not row["pending"] and not row["axioms"] for row in rows.values())


@pytest.mark.parametrize("stale_report", [False, True])
def test_probe_task_failure_stays_blocking_and_uncached(tmp_path, lean, stale_report):
    binary, env = lean
    source = tmp_path / "M.lean"
    source.write_text('import MissingLibrary\nnamespace demo\n'
                      '/-- [demo::good] -/\ndef good := 1\nend demo\n')
    output = tmp_path / "observation"
    output.mkdir()
    if stale_report:
        (output / "axioms-M.json").write_text(json.dumps({"schema": SCHEMA, "module": "M",
            "complete": True, "declarations": [], "error_lines": []}))
    args = dict(output=output, tools={"lean": binary}, env=env,
                environment="test-resources", dependencies={}, cache=Cache(tmp_path / "cache"))
    for _ in range(2):
        summary, step = observe_module(source, "M", **args)
        assert step["exit_code"] != 0 and not step["cached"]
        assert summary.get("error") and not summary.get("complete")
        assert Path(summary["report"]).exists() == stale_report
        assert "report_sha256" not in summary
        with pytest.raises(ValueError, match="Axiom observation unavailable") as failure:
            validate_export(observation(source, summary), tmp_path / "export")
        assert not isinstance(failure.value, NativeExportCandidateError)
    assert "Cannot inspect axioms: module header/imports failed" in Path(step["log"]).read_text()


def test_imported_and_elaborated_axioms_with_partial_coverage(tmp_path, lean):
    binary, env = lean
    library = tmp_path / "Support.lean"
    library.write_text('import Lean\nnamespace Support\naxiom Carrier : Type\n'
                       'axiom Unused : Prop\n'
                       'def literal (_s : String) (_h : 1 + 1 = 2 := by decide +native) : Nat := 1\n'
                       'end Support\n')
    subprocess.run([binary, "-o", str(library.with_suffix(".olean")), str(library)],
                   cwd=tmp_path, env=env, check=True, capture_output=True, text=True)
    source, summary, report = inspect(tmp_path, lean,
        'import Support\nnamespace demo\n'
        '/-- [demo::Packet] -/\nstructure Packet where\n  value : Support.Carrier\n'
        '/-- [demo::literal] -/\ndef literal := Support.literal "abc"\n'
        '/-- [demo::broken] -/\ndef broken : Nat := missingCarrier\n'
        '/-- [demo::dependent] -/\ndef dependent := broken\n'
        '/-- [demo::good] -/\ndef good := 7\nend demo\n')
    rows = {row["name"]: row for row in report["declarations"]}
    assert not report["complete"]
    assert rows["demo.broken"]["pending"] and rows["demo.dependent"]["pending"]
    assert not rows["demo.Packet"]["pending"] and not rows["demo.good"]["pending"]
    assert any("_native" in a for a in rows["demo.literal"]["axioms"])
    assert "Support.Carrier" in rows["demo.Packet"]["axioms"]
    obs = observation(source, summary)
    result = compatibility(obs, {"namespace": "demo", "global": []}, output=tmp_path / "compatibility.json")
    assert not result["complete"] and result["pending_declarations"] >= 2
    assert "Support.Carrier" in result["unapproved_axioms"]
    assert "Support.Unused" not in result["unapproved_axioms"]
    assert "sorryAx" not in result["unapproved_axioms"]
    assert any("_native" in name for name in result["unapproved_axioms"])
    with pytest.raises(NativeExportCandidateError, match="unapproved axioms"):
        validate_export(obs, tmp_path / "export")
    retained = json.loads((tmp_path / "export/axioms.json").read_text())
    assert set(retained["unapproved_axioms"]) == set(result["unapproved_axioms"])


def test_handoff_and_repair_keep_pending_distinct_from_approved(tmp_path, lean):
    text = ('namespace demo\n/-- [demo::broken] -/\ndef broken : MissingCarrier := 1\n'
            '/-- [demo::independent] -/\ndef independent := 2\nend demo\n')
    source, summary, report = inspect(tmp_path, lean, text)
    result = validate_export(observation(source, summary), tmp_path / "partial-export")
    assert result["passed"] and not result["axiom_compatibility"]["complete"]
    assert result["axiom_compatibility"]["unapproved_axioms"] == []
    source, summary, report = inspect(tmp_path, lean, text.replace("MissingCarrier", "Nat"))
    result = validate_export(observation(source, summary), tmp_path / "repaired-export")
    assert result["passed"] and result["axiom_compatibility"]["complete"]


def test_explicit_sorry_is_a_blocker_not_recovery(tmp_path, lean):
    source, summary, report = inspect(tmp_path, lean, 'namespace demo\ndef broken : Nat := sorry\nend demo\n')
    assert report["complete"]  # Successful elaboration, not successful trust checking.
    result = compatibility(observation(source, summary), {"namespace": "demo", "global": []},
                           output=tmp_path / "compatibility.json")
    assert result["unapproved_axioms"] == ["sorryAx"]


@pytest.mark.parametrize("change", ["source", "report", "probe", "environment"])
def test_changed_evidence_cannot_be_reused(tmp_path, lean, change):
    source, summary, _ = inspect(tmp_path, lean, 'namespace demo\ndef good := 1\nend demo\n')
    obs = observation(source, summary)
    if change == "source":
        source.write_text(source.read_text().replace("1", "2"))
    elif change == "report":
        Path(summary["report"]).write_text('{}')
    elif change == "probe":
        obs["axiom_dependencies"]["probe_sha256"] = "old"
    else:
        obs["environment"] = "different-resources"
    with pytest.raises(ValueError):
        compatibility(obs, {"namespace": "demo", "global": []}, output=tmp_path / "compatibility.json")


def test_policy_rechecked_without_reelaboration(tmp_path, lean):
    source, summary, _ = inspect(tmp_path, lean, 'namespace demo\naxiom boundary : Nat\ndef value := boundary\nend demo\n')
    obs = observation(source, summary)
    first = compatibility(obs, {"namespace": "demo", "global": ["boundary"]}, output=tmp_path / "first.json")
    assert first["unapproved_axioms"] == []
    second = compatibility(obs, {"namespace": "demo", "global": []}, output=tmp_path / "second.json")
    assert second["unapproved_axioms"] == ["demo.boundary"]


def test_probe_timeout_is_not_cached_and_retry_collects_evidence(tmp_path, lean, monkeypatch):
    from trellis.modeling import axioms
    binary, env = lean
    source = tmp_path / "M.lean"
    source.write_text('namespace demo\n/-- [demo::good] -/\ndef good := 1\nend demo\n')
    output = tmp_path / "observation"
    output.mkdir()
    args = dict(output=output, tools={"lean": binary}, env=env,
                environment="test-resources", dependencies={}, cache=Cache(tmp_path / "cache"))
    real_run = axioms.run_source_command
    def timeout(*args, **kwargs):
        kwargs["stdout"].write_text("test fixture: execution budget exhausted\n")
        return {"exit_code": 124, "seconds": 0.01, "timeout": True}
    monkeypatch.setattr(axioms, "run_source_command", timeout)
    summary, step = observe_module(source, "M", **args)
    assert summary["timeout"] and not step["cached"]
    with pytest.raises(NativeExportCandidateError, match="timed out"):
        validate_export(observation(source, summary), tmp_path / "timeout-export")
    monkeypatch.setattr(axioms, "run_source_command", real_run)
    summary, step = observe_module(source, "M", **args)
    assert not step["cached"] and not summary["timeout"] and not summary.get("error")
    assert validate_export(observation(source, summary), tmp_path / "retry-export")["passed"]
