"""Generic byte materialization and canonical source-tree guards."""

import importlib.util
import json
import pathlib
import sys
from types import SimpleNamespace

import pytest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = ROOT / "kernel" / "tests" / "fixtures" / "pv_generic_source"
sys.path.insert(0, str(ROOT))


def _load(name: str):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


ADAPT = _load("adapt_pv_source")
SOURCE_TREE = _load("pv_source_tree")
PRODUCTION_SANDBOX = _load("pv_source_adaptation_sandbox")


def test_source_tree_materialization_is_byte_identical(tmp_path: pathlib.Path) -> None:
    output = tmp_path / "copy"
    assert ADAPT.main(["--source-tree", str(SOURCE), "--out", str(output)]) == 0
    for source in sorted(SOURCE.rglob("*")):
        if source.is_file():
            relative = source.relative_to(SOURCE)
            assert (output / relative).read_bytes() == source.read_bytes()


def test_source_tree_materialization_is_deterministic(tmp_path: pathlib.Path) -> None:
    outputs = [tmp_path / "first", tmp_path / "second"]
    for output in outputs:
        assert ADAPT.main(["--source-tree", str(SOURCE), "--out", str(output)]) == 0
    assert {
        path.relative_to(outputs[0]): path.read_bytes()
        for path in outputs[0].rglob("*")
        if path.is_file()
    } == {
        path.relative_to(outputs[1]): path.read_bytes()
        for path in outputs[1].rglob("*")
        if path.is_file()
    }


def test_raw_module_tree_without_cargo_manifest_is_materialized(tmp_path: pathlib.Path) -> None:
    source = tmp_path / "raw-module"
    (source / "nested").mkdir(parents=True)
    expected = {
        "mod.rs": b"pub mod nested;\n\xff",
        "nested/impl.rs": b"pub fn raw() {}\r\n",
    }
    for relative, content in expected.items():
        path = source / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)

    output = tmp_path / "copy"
    assert ADAPT.main(["--source-tree", str(source), "--out", str(output)]) == 0
    assert {
        path.relative_to(output).as_posix(): path.read_bytes()
        for path in output.rglob("*") if path.is_file()
    } == expected
    assert SOURCE_TREE.source_tree_manifest(output) == SOURCE_TREE.source_tree_manifest(source)


def test_partitioned_materialization_preserves_paths_and_records_both_origins(
    tmp_path: pathlib.Path,
) -> None:
    target = tmp_path / "target"
    supporting = tmp_path / "supporting"
    (target / "module").mkdir(parents=True)
    supporting.mkdir()
    (target / "module/lib.rs").write_bytes(b"use crate::traits::Float;\n")
    (supporting / "traits.rs").write_bytes(b"pub trait Float {}\n")
    output = tmp_path / "union"
    tree_manifest_path = tmp_path / "tree.json"
    partition_manifest_path = tmp_path / "partitions.json"

    assert ADAPT.main(
        [
            "--target-tree", str(target),
            "--supporting-tree", str(supporting),
            "--out", str(output),
            "--tree-manifest-out", str(tree_manifest_path),
            "--partition-manifest-out", str(partition_manifest_path),
        ]
    ) == 0
    union = SOURCE_TREE.source_tree_manifest(output)
    partition = json.loads(partition_manifest_path.read_bytes())
    assert json.loads(tree_manifest_path.read_bytes()) == union
    assert [(row["relative_path"], row["origin"]) for row in partition["files"]] == [
        ("module/lib.rs", "target"),
        ("traits.rs", "supporting"),
    ]
    SOURCE_TREE.validate_source_partition_manifest(partition, union)


def test_partitioned_materialization_refuses_overlapping_paths(
    tmp_path: pathlib.Path,
) -> None:
    target = tmp_path / "target"
    supporting = tmp_path / "supporting"
    target.mkdir()
    supporting.mkdir()
    (target / "lib.rs").write_bytes(b"target\n")
    (supporting / "lib.rs").write_bytes(b"supporting\n")
    output = tmp_path / "union"
    with pytest.raises(SystemExit, match="uploads overlap.*lib.rs"):
        ADAPT.main(
            [
                "--target-tree", str(target),
                "--supporting-tree", str(supporting),
                "--out", str(output),
                "--tree-manifest-out", str(tmp_path / "tree.json"),
                "--partition-manifest-out", str(tmp_path / "partitions.json"),
            ]
        )
    assert not output.exists()


def test_materialization_refuses_existing_output_and_symlinks(tmp_path: pathlib.Path) -> None:
    output = tmp_path / "existing"
    output.mkdir()
    with pytest.raises(SystemExit, match="already exists"):
        ADAPT.main(["--source-tree", str(SOURCE), "--out", str(output)])

    source = tmp_path / "source"
    source.mkdir()
    (source / "Cargo.toml").write_text("[package]\nname='fixture'\nversion='0.1.0'\n")
    (source / "link").symlink_to("Cargo.toml")
    with pytest.raises(SystemExit, match="symlink"):
        ADAPT.main(["--source-tree", str(source), "--out", str(tmp_path / "linked")])
    assert not (tmp_path / "linked").exists()


def test_materialization_refuses_empty_and_special_trees(tmp_path: pathlib.Path) -> None:
    empty = tmp_path / "empty"
    empty.mkdir()
    with pytest.raises(SystemExit, match="no regular files"):
        ADAPT.main(["--source-tree", str(empty), "--out", str(tmp_path / "empty-copy")])
    assert not (tmp_path / "empty-copy").exists()

    if hasattr(__import__("os"), "mkfifo"):
        import os

        special = tmp_path / "special"
        special.mkdir()
        (special / "mod.rs").write_bytes(b"pub fn f() {}\n")
        os.mkfifo(special / "input.fifo")
        with pytest.raises(SystemExit, match="special file"):
            ADAPT.main(["--source-tree", str(special), "--out", str(tmp_path / "special-copy")])
        assert not (tmp_path / "special-copy").exists()


@pytest.mark.parametrize(
    "retired_args",
    [
        ["--config", "adaptation.json"],
        ["--manifest", "manifest.json"],
        ["--pinned-tree", "tree"],
        ["--patch", "fix.patch"],
        ["--verify"],
        ["--print-pinned-tree"],
    ],
)
def test_generic_materializer_rejects_retired_handwritten_modes(
    tmp_path: pathlib.Path, retired_args: list[str]
) -> None:
    with pytest.raises(SystemExit):
        ADAPT.main(
            [
                "--source-tree",
                str(SOURCE),
                "--out",
                str(tmp_path / "copy"),
                *retired_args,
            ]
        )


def test_production_extraction_uses_the_closed_source_adaptation_role(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    work = tmp_path / "work"
    home = tmp_path / "home"
    readonly = tmp_path / "source"
    for path in (work, home, readonly):
        path.mkdir()
    observed = {}

    def fake_wrap(command, **kwargs):
        observed.update(command=command, **kwargs)
        return ["/bin/true"]

    monkeypatch.setattr(PRODUCTION_SANDBOX, "wrap_command", fake_wrap)
    monkeypatch.setattr(
        PRODUCTION_SANDBOX.subprocess,
        "run",
        lambda *args, **kwargs: SimpleNamespace(returncode=0),
    )
    monkeypatch.setattr(
        sys,
        "argv",
        [
            "pv_source_adaptation_sandbox.py",
            "--work-dir", str(work),
            "--burst-home", str(home),
            "--readonly", str(readonly),
            "--", "/bin/true",
        ],
    )
    assert PRODUCTION_SANDBOX.main() == 0
    assert observed["role"] == "source_adaptation_checker"
    assert observed["sandbox"] == PRODUCTION_SANDBOX.SandboxConfig(
        enabled=True, backend="bwrap"
    )
    setup = (ROOT / "scripts/setup_pv_repo.sh").read_text(encoding="utf-8")
    assert '--opam-env-json "$OPAM_ENV_JSON"' in setup
    assert '--readonly "$_opam_switch_prefix"' in setup
    assert '--readonly "$OPAM_ENV_JSON"' in setup
    assert '--readonly "$FILESPEC_CHECKER"' in setup
    assert '--readonly "$PRESCRIBED_REGION_CHECKER"' in setup
    assert '--filespec-checker "$FILESPEC_CHECKER"' in setup
    assert '--prescribed-region-checker "$PRESCRIBED_REGION_CHECKER"' in setup
    assert "unset OPAMROOT" in setup
    assert '--readonly "$_opam_root"' not in setup
    assert 'CARGO_TARGET_DIR_RESOLVED="$WORK_ROOT/cargo-target"' in setup
    assert 'export CARGO_HOME="$WORK_ROOT/cargo-home"' in setup
    assert 'export TMPDIR="$WORK_ROOT/tmp" TMP="$WORK_ROOT/tmp" TEMP="$WORK_ROOT/tmp"' in setup
    assert 'export LD_LIBRARY_PATH="$_charon_toolchain_root/lib"' in setup
    assert "export CARGO_NET_OFFLINE=true" in setup
