"""Pinned paper bytes must survive the real authoritative workspace sync."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path

import pytest

from trellis.paper_implementation import SCHEMA, paper_input
from trellis.supervisor_workspace import _sync_repo_tree


def _write(path: Path, value: bytes | str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(value.encode() if isinstance(value, str) else value)


def _json(path: Path, value) -> None:
    _write(path, json.dumps(value))


def _seed(tmp_path: Path, phase: str = "native", route: str = "lean_native"):
    worker, supervisor = tmp_path / "worker", tmp_path / "supervisor"
    bundle = worker / "paper"
    content = {
        "main.tex": b"\\begin{theorem}Correct.\\end{theorem}\n",
        "references/target/results.json": b'{"count": 3}\n',
        "references/node_modules/catalogue.json": b"[]\n",
        "references/.lake/circuits/input.stim": b"H 0\nM 0\n",
        "references/.trellis/provenance.json": b"{}\n",
        "references/.tmp-download/raw.bin": b"\x00\xff\x80\r\n",
        "references/.tmp-manifest": b"temporary-looking original\n",
        "references/nested/.gitignore": b"*.json\n*.bin\n",
        "TRELLIS_REFERENCES.json": b'{"purpose":"independent circuit catalogue"}\n',
    }
    for name, data in content.items():
        _write(bundle / name, data)
    inventory = paper_input(bundle, "main.tex", [{"id": "main", "source_path": "main.tex",
        "start_line": 1, "end_line": 1, "selected_text": "Correct.", "excluded_clauses": []}],
        route=route, entry_points=[{"node_id": "Algorithm", "declaration": "Algorithm",
        "source_path": "Tablet/Algorithm.lean" if route == "lean_native" else "crate/src/lib.rs"}],
        implementation_roots={"main": ["Algorithm"]})
    if phase == "native":
        _json(worker / "trellis.config.json", {"paper_implementation": {
            "version": 1, "route": route, "manifest": "paper-implementation/manifest.json"}})
        _json(worker / "paper-implementation/manifest.json", {"schema": SCHEMA, **inventory,
            "source_files": ["Tablet/Algorithm.lean"], "build_files": [], "evidence_files": []})
    else:
        # These are the actual preparation identities, before a native manifest
        # exists: both languages keep paper_input in config.json.
        _json(worker / "trellis.config.json", {"workflow": {"paper_tex_path": "paper/main.tex"}})
        _json(worker / "config.json", {"paper_input": inventory, "tools": {}, "model": "fixture"})
        _json(worker / "paper-targets.json", inventory)
        _json(worker / ".trellis/modeling/state.json", {"verification_protocol":
            "paper-lean-preparation/v1" if route == "lean_native" else "native-v1"})
    supervisor.mkdir()
    return worker, supervisor, inventory


@pytest.mark.parametrize("phase", ["preparation", "native"])
@pytest.mark.parametrize("route", ["lean_native", "rust_aeneas"])
def test_real_sync_preserves_all_pinned_reference_paths_and_operational_caches(tmp_path, phase, route):
    worker, supervisor, inventory = _seed(tmp_path, phase, route)
    _write(worker / ".lake/build/worker-only", "skip worker build")
    _write(worker / "target/worker-only", "skip worker target")
    _write(worker / "node_modules/worker-only", "skip worker packages")
    _write(supervisor / ".lake/build/keep", "supervisor build")
    _write(supervisor / ".trellis/runtime/keep", "supervisor state")
    _write(supervisor / "isabelle/keep", "checker-owned")
    _write(supervisor / "paper/references/.lake/stale.bin", "stale")
    _write(supervisor / "paper/references/unused/.trellis/stale.bin", "stale")
    _write(supervisor / "paper/references/target/stale.bin", "stale")
    # Newly unpinned files in the worker do not gain authority in the mirror.
    _write(worker / "paper/references/unlisted.json", "not in original paper")

    result = _sync_repo_tree(worker, supervisor)

    assert result["verified_paper_files"] == len(inventory["paper_files"])
    expected = set(inventory["paper_files"])
    actual = {p.relative_to(supervisor).as_posix() for p in (supervisor / "paper").rglob("*") if p.is_file()}
    assert actual == expected
    for name in expected:
        assert (supervisor / name).read_bytes() == (worker / name).read_bytes()
        assert hashlib.sha256((supervisor / name).read_bytes()).hexdigest() == inventory["files"][name]
    assert not (supervisor / ".lake/build/worker-only").exists()
    assert not (supervisor / "target").exists()
    assert not (supervisor / "node_modules").exists()
    assert (supervisor / ".lake/build/keep").read_text() == "supervisor build"
    assert (supervisor / ".trellis/runtime/keep").read_text() == "supervisor state"
    assert (supervisor / "isabelle/keep").read_text() == "checker-owned"


def test_pinned_copy_repairs_same_size_same_mtime_corruption(tmp_path):
    worker, supervisor, inventory = _seed(tmp_path)
    _sync_repo_tree(worker, supervisor)
    name = "paper/references/target/results.json"
    source, destination = worker / name, supervisor / name
    destination.write_bytes(b"x" * source.stat().st_size)
    stat = source.stat()
    os.utime(destination, ns=(stat.st_atime_ns, stat.st_mtime_ns))
    _sync_repo_tree(worker, supervisor)
    assert destination.read_bytes() == source.read_bytes()
    assert hashlib.sha256(destination.read_bytes()).hexdigest() == inventory["files"][name]


@pytest.mark.parametrize("missing", [False, True])
def test_changed_or_missing_source_pin_fails_before_authoritative_mutation(tmp_path, missing):
    worker, supervisor, inventory = _seed(tmp_path, "preparation")
    _write(supervisor / "untouched", "prior authoritative state")
    source = worker / inventory["paper_files"][0]
    source.unlink() if missing else source.write_bytes(b"changed")
    with pytest.raises(ValueError, match="Missing evidence|Original paper input changed"):
        _sync_repo_tree(worker, supervisor)
    assert (supervisor / "untouched").read_text() == "prior authoritative state"
    assert not (supervisor / "paper").exists()


def test_source_alias_and_destination_alias_cannot_satisfy_a_pin(tmp_path):
    worker, supervisor, inventory = _seed(tmp_path)
    name = "paper/references/target/results.json"
    source = worker / name
    original = source.read_bytes()
    outside = tmp_path / "outside.json"
    outside.write_bytes(original)
    source.unlink()
    source.symlink_to(outside)
    with pytest.raises(ValueError, match="symlinks"):
        _sync_repo_tree(worker, supervisor)
    source.unlink()
    source.write_bytes(original)
    (supervisor / "paper").symlink_to(tmp_path)
    with pytest.raises(ValueError, match="destination may not use symlinks"):
        _sync_repo_tree(worker, supervisor)
    assert outside.read_bytes() == original


def test_generic_relative_pin_is_verified_without_relaxing_unrelated_cache_exclusions(tmp_path):
    worker, supervisor, inventory = _seed(tmp_path)
    generic = "docs/.lake/reference.json"
    _write(worker / generic, "ordinary relative paper evidence")
    inventory["paper_files"].append(generic)
    inventory["files"][generic] = hashlib.sha256((worker / generic).read_bytes()).hexdigest()
    _json(worker / "paper-implementation/manifest.json", {"schema": SCHEMA, **inventory})
    _write(worker / "docs/.lake/unlisted-cache", "skip")
    _sync_repo_tree(worker, supervisor)
    assert (supervisor / generic).read_bytes() == (worker / generic).read_bytes()
    assert not (supervisor / "docs/.lake/unlisted-cache").exists()
    # Reject a prior destination alias before the ordinary source loop can
    # write through it, including a generic non-cache path such as docs/main.
    generic = "other/main.tex"
    _write(worker / generic, "original")
    inventory["paper_files"].append(generic)
    inventory["files"][generic] = hashlib.sha256((worker / generic).read_bytes()).hexdigest()
    _json(worker / "paper-implementation/manifest.json", {"schema": SCHEMA, **inventory})
    outside = tmp_path / "outside"
    outside.mkdir()
    _write(outside / "main.tex", "outside")
    (supervisor / "other").symlink_to(outside)
    with pytest.raises(ValueError, match="destination may not use symlinks"):
        _sync_repo_tree(worker, supervisor)
    assert (outside / "main.tex").read_text() == "outside"


@pytest.mark.parametrize("name", [".lake/paper.tex", ".trellis/paper.tex", "isabelle/paper.tex", ".trellis-supervisor.json"])
def test_paper_pin_cannot_overwrite_authoritative_operational_roots(tmp_path, name):
    worker, supervisor, inventory = _seed(tmp_path)
    _write(worker / name, "paper content")
    inventory["paper_files"].append(name)
    inventory["files"][name] = hashlib.sha256((worker / name).read_bytes()).hexdigest()
    _json(worker / "paper-implementation/manifest.json", {"schema": SCHEMA, **inventory})
    with pytest.raises(ValueError, match="overlaps authoritative operational root"):
        _sync_repo_tree(worker, supervisor)
    assert not (supervisor / name).exists()


@pytest.mark.parametrize("phase", ["native", "preparation"])
def test_malformed_paper_identity_fails_closed(tmp_path, phase):
    worker, supervisor, _ = _seed(tmp_path, phase)
    path = "paper-implementation/manifest.json" if phase == "native" else "config.json"
    (worker / path).write_text("not JSON")
    with pytest.raises(ValueError):
        _sync_repo_tree(worker, supervisor)
    assert not (supervisor / "paper").exists()


def test_ordinary_math_pv_sync_ignores_unrelated_non_json_config_and_keeps_exclusions(tmp_path):
    worker, supervisor = tmp_path / "worker", tmp_path / "supervisor"
    _write(worker / "config.json", "unrelated non-JSON application configuration")
    _write(worker / "trellis.config.json", '{"workflow":{"paper_tex_path":"paper.tex"}}')
    _write(worker / "paper.tex", "ordinary math paper")
    _write(worker / "paper/references/target/not-pinned", "ordinary excluded cache")
    _write(worker / "paper/references/visible.txt", "ordinary visible file")
    _write(supervisor / ".lake/build/keep", "cached")
    _sync_repo_tree(worker, supervisor)
    assert (supervisor / "config.json").read_bytes() == (worker / "config.json").read_bytes()
    assert (supervisor / "paper.tex").read_text() == "ordinary math paper"
    assert not (supervisor / "paper/references/target/not-pinned").exists()
    assert (supervisor / "paper/references/visible.txt").read_text() == "ordinary visible file"
    assert (supervisor / ".lake/build/keep").read_text() == "cached"


@pytest.mark.parametrize("name", [".git/config", "paper/references/.gitattributes"])
def test_reserved_git_control_paths_cannot_become_paper_sync_inputs(tmp_path, name):
    worker, supervisor, inventory = _seed(tmp_path)
    _write(worker / name, "reserved control content")
    inventory["paper_files"].append(name)
    inventory["files"][name] = hashlib.sha256((worker / name).read_bytes()).hexdigest()
    _json(worker / "paper-implementation/manifest.json", {"schema": SCHEMA, **inventory})
    with pytest.raises(ValueError, match="Reserved Git control path"):
        _sync_repo_tree(worker, supervisor)
    assert not (supervisor / name).exists()
