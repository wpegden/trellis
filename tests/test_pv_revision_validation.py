"""Source checks execute against isolated exact bases and preserve receipts."""
import hashlib
from pathlib import Path
import sys

import pytest

from trellis.pv_revision.validation import ProductionChecker


def test_regression_checks_compare_private_source_copies(tmp_path, monkeypatch):
    root = tmp_path / "revision"
    original, candidate = root / "sources/original", root / "sources/candidate"
    for path, value in ((original, "wrong"), (candidate, "correct")):
        path.mkdir(parents=True)
        (path / "value").write_text(value)
    wrapped = []

    def sandbox(argv, **kwargs):
        wrapped.append(kwargs)
        return argv  # These test-owned programs exercise receipts, not bwrap.

    monkeypatch.setattr("trellis.pv_revision.validation.wrap_command", sandbox)
    command = [sys.executable, "-c", "import pathlib,sys; p=pathlib.Path(sys.argv[1]); "
               "s=p.read_text(); p.write_text('scratch edit'); print(s); sys.exit(0 if s=='correct' else 1)",
               "{source}/value"]
    result = ProductionChecker(root, {})(candidate, {
        "id": "one", "base_source": "sources/original", "authorization": {
            "checks": [{"id": "regression", "argv": command, "expected_exit": 0, "baseline_expected_exit": 1}]}})
    assert result["ok"]
    assert [entry["side"] for entry in result["checks"]] == ["candidate", "baseline"]
    assert all(entry["role"] == "source_adaptation_checker" for entry in wrapped)
    assert (original / "value").read_text() == "wrong"
    assert (candidate / "value").read_text() == "correct"
    for path, digest in result["artifacts"].items():
        assert hashlib.sha256(Path(path).read_bytes()).hexdigest() == digest
    assert hashlib.sha256(Path(result["evidence"]).read_bytes()).hexdigest() == result["evidence_sha256"]


def test_missing_checks_cannot_report_success(tmp_path):
    source = tmp_path / "sources/original"
    source.mkdir(parents=True)
    (source / "lib.rs").write_text("source")
    result = ProductionChecker(tmp_path, {})(source, {"id": "none", "authorization": {}})
    assert not result["ok"] and result["checks"] == []


def test_checker_rejects_external_baseline(tmp_path):
    source = tmp_path / "sources/original"
    source.mkdir(parents=True)
    with pytest.raises(ValueError, match="baseline leaves"):
        ProductionChecker(tmp_path, {})(source, {"id": "bad", "base_source": "/tmp",
            "authorization": {"checks": [{"id": "noop", "argv": ["true"]}]}})


def test_new_regression_harness_runs_against_unchanged_baseline(tmp_path, monkeypatch):
    root = tmp_path / 'revision'
    for name, value in [('original', 'old'), ('candidate', 'new')]:
        source = root / 'sources' / name
        source.mkdir(parents=True)
        (source / 'value').write_text(value)
    candidate = root / 'sources/candidate'
    (candidate / 'regression.py').write_text(
        'import pathlib,sys\nvalue=pathlib.Path(sys.argv[1]).read_text()\n'
        'assert not (pathlib.Path(sys.argv[1]).parent / "regression.py").exists()\n'
        'print(value)\nsys.exit(0 if value=="new" else 1)\n')
    monkeypatch.setattr('trellis.pv_revision.validation.wrap_command', lambda argv, **kw: argv)
    result = ProductionChecker(root, {})(candidate, {'id': 'harness', 'base_source': 'sources/original',
        'authorization': {'checks': [{'id': 'regression', 'harness_paths': ['regression.py'],
            'argv': [sys.executable, '{harness}/regression.py', '{source}/value'],
            'expected_exit': 0, 'baseline_expected_exit': 1}]}})
    assert result['ok']
    assert result['checks'][0]['harness_manifest'] == result['checks'][1]['harness_manifest']
    assert all([row['relative_path'] for row in check['production_manifest']['files']] == ['value'] for check in result['checks'])
    assert not (root / 'sources/original/regression.py').exists()
