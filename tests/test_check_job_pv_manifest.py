import base64
import json

import pytest

from trellis.checker.job_workspace import capture, compiler_command, prepare_mirror


def source_repo(tmp_path):
    repo = tmp_path / "source"
    (repo / "modeling-input").mkdir(parents=True)
    (repo / "Tablet").mkdir()
    (repo / "Tablet/A.lean").write_text("theorem A : True := by trivial\n")
    (repo / "lean-toolchain").write_text("leanprover/lean4:v4.30.0-rc2\n")
    return repo


def test_capture_pv_manifest_exact_bytes_and_selection_epoch(tmp_path):
    repo = source_repo(tmp_path)
    manifest = repo / "modeling-input/manifest.json"
    original = b'{"effective_tools": {"lean_paths": ["/selected/one"]}}\n'
    manifest.write_bytes(original)
    for rel in ["arbitrary.json", "modeling-input/other.json", "Tablet/manifest.json"]:
        (repo / rel).write_text('{"not": "an input"}')
    first, data = capture(repo)
    inputs = json.loads(data)
    assert base64.b64decode(inputs["modeling-input/manifest.json"]) == original
    assert {name for name in inputs if name.endswith(".json")} == {"modeling-input/manifest.json"}
    (repo / "modeling-input/other.json").write_text("changed unrelated metadata")
    assert capture(repo)[0] == first
    manifest.write_bytes(original.replace(b"/selected/one", b"/selected/two"))
    assert capture(repo)[0] != first
    assert set(json.loads(capture(repo, metadata_only=True)[1])) == {"lean-toolchain"}


@pytest.mark.parametrize("directory", [False, True])
def test_capture_pv_manifest_rejects_symlink_input(tmp_path, directory):
    repo = source_repo(tmp_path)
    external = tmp_path / "external"
    external.mkdir()
    (external / "manifest.json").write_text('{"effective_tools": {"lean_paths": []}}')
    if directory:
        (repo / "modeling-input").rmdir()
        (repo / "modeling-input").symlink_to(external, target_is_directory=True)
    else:
        (repo / "modeling-input/manifest.json").symlink_to(external / "manifest.json")
    with pytest.raises(ValueError, match="symlink in check input|regular file"):
        capture(repo)


def test_advisory_mirror_uses_selected_readonly_library_after_package_bind(tmp_path, monkeypatch):
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    repo = source_repo(tmp_path)
    selected = tmp_path / "selected/mathlib/.lake/build/lib/lean"
    selected.mkdir(parents=True)
    (selected / "Example.olean").write_bytes(b"selected immutable library")
    local = repo / ".lake/packages/mathlib/.lake/build/lib/lean"
    local.mkdir(parents=True)
    (local / "Example.olean").write_bytes(b"stale local alias")
    (repo / "modeling-input/manifest.json").write_text(json.dumps({
        "effective_tools": {"lean_paths": [str(selected)]},
    }))
    mirror = tmp_path / "workspace/repo"
    _, snapshot = capture(repo)
    prepare_mirror(repo, mirror, snapshot)
    assert (mirror / "modeling-input/manifest.json").read_bytes() == (repo / "modeling-input/manifest.json").read_bytes()
    args = compiler_command(mirror, ["lake", "build", "Tablet.A"])
    binds = [(i, args[i], args[i + 1], args[i + 2])
             for i in range(len(args) - 2) if args[i] in {"--bind", "--ro-bind"}]
    packages = mirror / ".lake/packages"
    alias = packages / "mathlib/.lake/build/lib/lean"
    package_ro = next(i for i, mode, src, dst in binds
                      if mode == "--ro-bind" and src == dst == str(packages))
    library_ro = next(i for i, mode, src, dst in binds
                      if mode == "--ro-bind" and src == str(selected) and dst == str(alias))
    assert library_ro > package_ro
    assert all(i < package_ro for i, mode, src, dst in binds
               if mode == "--bind" and (dst == str(packages) or dst.startswith(str(packages) + "/")))
    assert not any(i > library_ro and dst == str(alias) for i, mode, src, dst in binds)
    assert (local / "Example.olean").read_bytes() == b"stale local alias"
    assert (selected / "Example.olean").read_bytes() == b"selected immutable library"
