"""Campaign publication is evidence export, not a new verification decision."""
import json

import pytest

from scripts import pv_native_create as native
from scripts import pv_revision as cli


@pytest.mark.parametrize("outcome", ["complete", "all_requirements_retired"])
def test_terminal_callback_exports_even_without_revision_handoff(tmp_path, monkeypatch, outcome):
    from trellis.pv_revision import archive, lifecycle
    runtime = tmp_path / "runtime"
    runtime.mkdir()
    evidence = runtime / "protocol_state.json"
    evidence.write_text('{"phase":"Complete"}\n')
    before = evidence.read_bytes()
    monkeypatch.setattr(lifecycle, "continue_runtime", lambda *a, **k: False)
    monkeypatch.setattr(archive, "campaign_status", lambda _: {"terminal_outcome": outcome, "can_export_final": True})
    published = []
    monkeypatch.setattr(archive, "export_campaign", lambda root: published.append(root))
    assert native.continue_native(runtime)
    assert published == [runtime]
    assert evidence.read_bytes() == before


def test_publication_failure_is_recorded_and_retriable_without_changing_verdict(tmp_path, monkeypatch):
    from trellis.pv_revision import archive
    runtime = tmp_path / "runtime"
    runtime.mkdir()
    evidence = runtime / "protocol_state.json"
    evidence.write_text('{"phase":"Complete"}\n')
    before = evidence.read_bytes()
    monkeypatch.setattr(archive, "campaign_status", lambda _: {
        "terminal_outcome": "complete", "can_export_final": False,
        "error": "preserved proof unavailable"})
    def failed(root):
        raise ValueError("preserved proof unavailable")
    monkeypatch.setattr(archive, "export_campaign", failed)
    with pytest.raises(ValueError, match="preserved proof unavailable"):
        native.export_completed_campaign(runtime)
    receipt = runtime / "campaign-archives/last-error.json"
    assert json.loads(receipt.read_text())["error"] == "preserved proof unavailable"
    assert evidence.read_bytes() == before
    monkeypatch.setattr(archive, "export_campaign", lambda root: None)
    assert native.export_completed_campaign(runtime)
    assert not receipt.exists()
    assert evidence.read_bytes() == before


@pytest.mark.parametrize("status", [None, {"terminal_outcome": "in_progress", "can_export_final": False}])
def test_unfinished_or_non_native_run_does_not_publish(tmp_path, monkeypatch, status):
    from trellis.pv_revision import archive
    monkeypatch.setattr(archive, "campaign_status", lambda _: status)
    monkeypatch.setattr(archive, "export_campaign", lambda *_: pytest.fail("unfinished final export"))
    assert not native.export_completed_campaign(tmp_path)


def test_campaign_cli_reports_exact_export_hash_and_explicit_preview(tmp_path, monkeypatch, capsys):
    import hashlib
    from trellis.pv_revision import archive
    path = tmp_path / "history.zip"
    path.write_bytes(b"preserved archive bytes")
    calls = []
    def export(root, *, allow_incomplete=False):
        calls.append((root, allow_incomplete))
        return path
    monkeypatch.setattr(archive, "export_campaign", export)
    assert cli.main(["campaign-export", str(tmp_path), "--allow-incomplete"]) == 0
    assert calls == [(tmp_path, True)]
    assert json.loads(capsys.readouterr().out) == {
        "path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}


def test_campaign_cli_does_not_claim_failed_export(tmp_path, monkeypatch, capsys):
    from trellis.pv_revision import archive
    def export(*args, **kwargs):
        raise ValueError("missing predecessor evidence")
    monkeypatch.setattr(archive, "export_campaign", export)
    assert cli.main(["campaign-export", str(tmp_path)]) == 1
    assert json.loads(capsys.readouterr().out) == {"error": "missing predecessor evidence"}
