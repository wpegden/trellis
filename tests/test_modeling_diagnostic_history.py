"""Worker diagnostics must not write the controller's protected history."""
from pathlib import Path
import shutil

import pytest

from scripts.pv_modeling import _main
from trellis.modeling.history import HISTORY
from trellis.modeling.observations import observe, passed, write_json


@pytest.mark.parametrize("command", ["check", "probe"])
def test_worker_check_keeps_history_with_scratch_evidence(tmp_path, monkeypatch, command):
    source = tmp_path / "source"
    source.mkdir()
    (source / "lib.rs").write_text("pub fn f() {}\n")
    (source / "Cargo.toml").write_text('[package]\nname="fixture"\nversion="0.1.0"\n')
    goals = tmp_path / "goals.json"
    write_json(goals, {"goals": [{"id": "g", "text": "goal"}]})
    root = tmp_path / "run"
    shutil.copytree(source, root / "work/crate")
    shutil.copytree(source, root / "upstream")
    write_json(root / "config.json", {"tools": {name: "/bin/true" for name in ("cargo", "charon", "aeneas", "lean")}})
    write_json(root / "work/extraction.json", {"crate": "crate"})
    protected = root / HISTORY
    before = {str(p.relative_to(protected)): p.read_bytes() for p in protected.rglob("*") if p.is_file()}
    mkdir = Path.mkdir
    def protected_mkdir(path, *args, **kwargs):
        if path.is_relative_to(protected):
            raise PermissionError("worker history is read-only")
        return mkdir(path, *args, **kwargs)
    with monkeypatch.context() as patch:
        patch.setattr(Path, "mkdir", protected_mkdir)
        args = [command, str(root), "--through", "cargo"]
        args += ["--fresh"] if command == "check" else ["--source", str(source)]
        assert _main(args) == 0
    assert before == {str(p.relative_to(protected)): p.read_bytes() for p in protected.rglob("*") if p.is_file()}
    assert list((root / ".trellis/scratch").glob("**/history/blobs/*"))
    # The controller's direct observation retains its canonical destination.
    assert passed(observe(root, root / "observations/host", through="cargo"), "cargo")
    assert before != {str(p.relative_to(protected)): p.read_bytes() for p in protected.rglob("*") if p.is_file()}
