import json
import shutil
import subprocess
from pathlib import Path

import pytest

from trellis.acceptance_identity import source_manifest, source_identity, record_deployment

ROOT = Path(__file__).resolve().parents[1]
KERNEL = ROOT / "kernel/target/debug/trellis_runtime_cli"


@pytest.fixture
def snapshot(tmp_path):
    for entry in source_manifest(ROOT)["sources"]:
        relative = entry["path"]
        path = tmp_path / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / relative, path)
    return tmp_path


@pytest.mark.parametrize("relative", ["kernel/src/tablet_support.rs", "kernel/src/model_preservation.rs",
                                     "trellis/modeling/native.py", "trellis/pv_model_support.py",
                                     "scripts/lean_preparation_axioms.lean", "trellis/modeling/axioms.py",
                                     "scripts/lean_local_closure.lean", "kernel/Cargo.lock"])
def test_rule_changes_change_acceptance_identity(snapshot, relative):
    before = source_identity(snapshot)
    assert not before.startswith("unavailable:")
    p = snapshot / relative
    p.write_text(p.read_text() + "\nchanged rule\n")
    assert source_identity(snapshot) != before


def test_source_membership_and_transient_files(snapshot):
    before = source_identity(snapshot)
    (snapshot / "trellis/.#editing.py").write_text("transient")
    assert source_identity(snapshot) == before
    p = snapshot / "trellis/new_gate.py"
    p.write_text("def new_gate(): pass\n")
    assert source_identity(snapshot) != before
    p.unlink()
    assert source_identity(snapshot) == before
    (snapshot / "kernel/Cargo.toml").unlink()
    assert source_identity(snapshot).startswith("unavailable:")


@pytest.mark.parametrize("failure", ["nonzero", "malformed", "missing", "list", "timeout", "oserror"])
def test_deployment_query_failure_refuses_execution(snapshot, tmp_path, monkeypatch, failure):
    binary = tmp_path / "fake-kernel"
    binary.write_bytes(b"dummy binary")
    def query(*args, **kwargs):
        if failure == "timeout":
            raise subprocess.TimeoutExpired(args[0], 15)
        if failure == "oserror":
            raise OSError("unavailable executable")
        output = {"nonzero": '{"identity":"trellis-worker-acceptance-v2:wrong"}',
                  "malformed": "broken json", "missing": "{}", "list": "[]"}[failure]
        return subprocess.CompletedProcess(args[0], 1 if failure == "nonzero" else 0, output, "")
    monkeypatch.setattr("trellis.acceptance_identity.subprocess.run", query)
    receipt = tmp_path / "receipt.json"
    with pytest.raises(RuntimeError, match="Acceptance implementation mismatch"):
        record_deployment(snapshot, binary, receipt)
    assert json.loads(receipt.read_text())["verified"] is False


@pytest.mark.skipif(not KERNEL.exists(), reason="build the runtime kernel for cross-language identity verification")
def test_compiled_kernel_and_deployed_checker_identity_agree(snapshot, tmp_path):
    build = json.loads(subprocess.check_output([str(KERNEL)], input=b'{"action":"acceptance_implementation_identity"}'))
    expected = source_manifest(ROOT)
    assert build["identity"] == expected["identity"]
    assert build["sources"] == expected["sources"]
    receipt = tmp_path / "receipt.json"
    assert record_deployment(snapshot, KERNEL, receipt)["verified"]
    (snapshot / "trellis/pv_model_support.py").write_text("changed implementation")
    with pytest.raises(RuntimeError, match="Acceptance implementation mismatch"):
        record_deployment(snapshot, KERNEL, receipt)
    assert not json.loads(receipt.read_text())["verified"]
