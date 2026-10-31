from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import shutil
import stat
import subprocess
from pathlib import Path

import pytest
from trellis.config import SandboxConfig


ROOT = Path(__file__).resolve().parents[1]
RUNNER_PATH = ROOT / "scripts" / "pv_phase0_checker_runner.py"
SPEC = importlib.util.spec_from_file_location("pv_phase0_checker_runner", RUNNER_PATH)
assert SPEC and SPEC.loader
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _program(path: Path, source: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(source, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


def _clean_checkout(root: Path) -> None:
    subprocess.run(["git", "init", "-q", str(root)], check=True)
    subprocess.run(["git", "-C", str(root), "config", "user.email", "fixture@example.invalid"], check=True)
    subprocess.run(["git", "-C", str(root), "config", "user.name", "Fixture"], check=True)
    subprocess.run(["git", "-C", str(root), "add", "."], check=True)
    subprocess.run(["git", "-C", str(root), "commit", "-q", "-m", "fixture"], check=True)
    subprocess.run(["git", "-C", str(root), "remote", "add", "origin", "https://example.invalid/tool"], check=True)


# An extra boundary block the fake backend emits when a fixture asks for a
# model that leaves a computation without a definition.
_BODILESS_BLOCK = (
    "\n/-- [fixture::helper]:\n"
    "    Source: \\'src/lib.rs\\', lines 3:0-5:1 -/\n"
    "axiom fixture.helper (x : Std.U64) : Result Std.U64\n"
)


def _fixture(
    tmp_path: Path,
    *,
    cargo_mode: str = "normal",
    failure_stage: str | None = None,
    bodiless: bool = False,
) -> dict:
    candidate = tmp_path / "candidate"
    (candidate / "src").mkdir(parents=True)
    (candidate / "Cargo.toml").write_text(
        '[package]\nname="fixture"\nversion="0.1.0"\nedition="2021"\n',
        encoding="utf-8",
    )
    (candidate / "Cargo.lock").write_text(
        'version = 4\n\n[[package]]\nname = "fixture"\nversion = "0.1.0"\n', encoding="utf-8"
    )
    (candidate / "src/lib.rs").write_text("pub fn f(x: u64) -> u64 { x }\n", encoding="utf-8")
    goal = tmp_path / "GOAL.md"
    goal.write_bytes(b"# target\n")
    output = tmp_path / "output"
    output.mkdir()

    toolchain = tmp_path / "toolchain"
    cargo_behavior = {
        "normal": (
            " target=pathlib.Path(args[args.index('--target-dir')+1])\n"
            " dep=target/'debug/deps/fixture.d'; dep.parent.mkdir(parents=True,exist_ok=True)\n"
            " source=pathlib.Path.cwd()/'src/lib.rs'\n"
            " dep.write_text(f'{dep}: {source}\\n')\n"
        ),
        "timeout": " import time\n time.sleep(3)\n",
        "truncate": " print('x' * 10000)\n",
    }[cargo_mode]
    metadata_behavior = (
        " print('metadata failed', file=sys.stderr)\n sys.exit(13)\n"
        if failure_stage == "cargo_metadata_cfg"
        else " manifest=str(pathlib.Path.cwd()/'Cargo.toml')\n print(json.dumps({'packages':[{'id':'fixture 0.1.0','name':'fixture','version':'0.1.0','manifest_path':manifest,'source':None}], 'resolve':{'nodes':[{'id':'fixture 0.1.0','features':[]}]}}))\n sys.exit(0)\n"
    )
    cfg_behavior = (
        " print('cfg failed', file=sys.stderr)\n sys.exit(14)\n"
        if failure_stage == "cargo_cfg"
        else " print('overflow_checks')\n print('panic=\"unwind\"')\n print('target_pointer_width=\"64\"')\n sys.exit(0)\n"
    )
    filespec_exit = 15 if failure_stage == "filespec_region_gates" else 0
    filespec_checker = _program(
        tmp_path / "kernel-checkers/scan_tablet_filespec",
        f"#!/usr/bin/python3\nimport sys\nsys.exit({filespec_exit})\n",
    )
    prescribed_region_checker = _program(
        tmp_path / "kernel-checkers/print_prescribed_region",
        "#!/usr/bin/python3\n"
        "import pathlib,sys\n"
        "s=pathlib.Path(sys.argv[1]).read_text(); lines=s.splitlines()\n"
        "i=next(i for i,x in enumerate(lines) if x.strip().startswith('-- [TABLET NODE:'))\n"
        "out='\\n'.join(x for x in lines[i+1:] if x.strip() != '-- BODY').strip('\\n')\n"
        "sys.stdout.write(out)\n",
    )
    cargo = _program(
        toolchain / "bin/cargo",
        "#!/usr/bin/python3\n"
        "import json, os, pathlib, stat, sys\n"
        "args=sys.argv[1:]\n"
        "assert os.environ.get('CARGO_NET_OFFLINE') == 'true', 'Cargo could reach a registry'\n"
        "if args and args[0] == 'check':\n"
        f"{cargo_behavior}"
        " sys.exit(0)\n"
        "if args and args[0] == 'metadata':\n"
        f"{metadata_behavior}"
        "if args and args[0] == 'rustc':\n"
        f"{cfg_behavior}"
        "if args and args[0] == 'build' and '--example' in args:\n"
        " name=args[args.index('--example')+1]\n"
        " target=pathlib.Path(os.environ['CARGO_TARGET_DIR'])/'debug/examples'/name\n"
        " target.parent.mkdir(parents=True, exist_ok=True)\n"
        " if name == 'scan_tablet_filespec':\n"
        f"  text='#!/usr/bin/python3\\nimport sys\\nsys.exit({filespec_exit})\\n'\n"
        " else:\n"
        "  text=\"#!/usr/bin/python3\\nimport pathlib,sys\\ns=pathlib.Path(sys.argv[1]).read_text()\\nlines=s.splitlines()\\ni=next(i for i,x in enumerate(lines) if x.strip().startswith('-- [TABLET NODE:'))\\nout='\\\\n'.join(x for x in lines[i+1:] if x.strip() != '-- BODY').strip('\\\\n')\\nsys.stdout.write(out)\\n\"\n"
        " target.write_text(text)\n"
        " target.chmod(target.stat().st_mode | stat.S_IXUSR)\n"
        " sys.exit(0)\n"
        "sys.exit(2)\n",
    )
    rustc = _program(
        toolchain / "bin/rustc",
        (
            "#!/bin/sh\nprintf 'rust identity failed\\n' >&2\nexit 12\n"
            if failure_stage == "rust_identity"
            else "#!/bin/sh\nprintf '%s\\n' 'rustc fixture' 'host: x86_64-unknown-linux-gnu'\n"
        ),
    )
    _program(toolchain / "bin/opam", "#!/bin/sh\nprintf '((PATH \"/usr/bin:/bin\"))\\n'\n")

    charon_root = tmp_path / "charon-source"
    charon = _program(
        charon_root / "bin/charon",
        (
            "#!/usr/bin/python3\nimport sys\nprint('charon failed', file=sys.stderr)\nsys.exit(16)\n"
            if failure_stage == "charon"
            else "#!/usr/bin/python3\nimport pathlib,sys\na=sys.argv\np=pathlib.Path(a[a.index('--dest-file')+1]); p.parent.mkdir(parents=True,exist_ok=True); p.write_bytes(b'llbc')\n"
        ),
    )
    _clean_checkout(charon_root)
    aeneas_root = tmp_path / "aeneas-source"
    aeneas_source = (
        "#!/usr/bin/python3\nimport sys\nprint('aeneas failed', file=sys.stderr)\nsys.exit(17)\n"
        if failure_stage == "aeneas"
        else "#!/usr/bin/python3\n"
        "import pathlib,sys\n"
        "a=sys.argv; out=pathlib.Path(a[a.index('-dest')+1]); out.mkdir(parents=True,exist_ok=True)\n"
        + (
            "(out/'Fixture.lean').write_text('not generated Lean')\n"
            if failure_stage == "generated_model_split"
            else "(out/'Fixture.lean').write_text('''-- THIS FILE WAS AUTOMATICALLY GENERATED BY AENEAS\n-- [fixture]\nimport Aeneas\nopen Aeneas Aeneas.Std Result ControlFlow Error\nset_option linter.dupNamespace false\n\nnamespace fixture\n\n/-- [fixture::f]:\n    Source: \'src/lib.rs\', lines 1:0-1:32 -/\ndef f (x : Std.U64) : Result Std.U64 := do\n  ok x\n" + (_BODILESS_BLOCK if bodiless else "") + "\nend fixture\n''')\n"
        )
    )
    aeneas = _program(aeneas_root / "bin/aeneas", aeneas_source)
    _clean_checkout(aeneas_root)
    opam_switch_prefix = tmp_path / "opam-switch"
    for relative in (
        "bin",
        "lib/stublibs",
        "lib/ocaml/stublibs",
        "lib/ocaml",
        "lib/toplevel",
    ):
        (opam_switch_prefix / relative).mkdir(parents=True, exist_ok=True)
    (opam_switch_prefix / "identity").write_text("fixture\n", encoding="utf-8")
    opam_switch_environment = {
        "OPAM_SWITCH_PREFIX": str(opam_switch_prefix),
        "OPAMSWITCH": "fixture",
        "CAML_LD_LIBRARY_PATH": os.pathsep.join(
            str(opam_switch_prefix / relative)
            for relative in ("lib/stublibs", "lib/ocaml/stublibs", "lib/ocaml")
        ),
        "OCAML_TOPLEVEL_PATH": str(opam_switch_prefix / "lib/toplevel"),
        "OCAMLTOP_INCLUDE_PATH": str(opam_switch_prefix / "lib/toplevel"),
    }

    manifest = RUNNER.source_tree_manifest(candidate)
    return {
        "schema": RUNNER.REQUEST_SCHEMA,
        "phase0_genesis_sha256": _sha(b"genesis"),
        "check_id": "check-1",
        "check_kind": "candidate",
        "ledger_generation": 1,
        "ledger_root_sha256": _sha(b"ledger"),
        "candidate_root": str(candidate),
        "candidate_manifest": manifest,
        "candidate_tree_sha256": manifest["source_tree_sha256"],
        "repository_root": str(ROOT),
        "goal_path": str(goal),
        "goal_sha256": _sha(goal.read_bytes()),
        "reverted_entry_id": None,
        "runner_path": str(RUNNER_PATH),
        "runner_sha256": _sha(RUNNER_PATH.read_bytes()),
        "extractor_path": str(ROOT / "scripts/extract_pv_model.py"),
        "extractor_sha256": _sha((ROOT / "scripts/extract_pv_model.py").read_bytes()),
        "filespec_checker_path": str(filespec_checker),
        "filespec_checker_sha256": _sha(filespec_checker.read_bytes()),
        "prescribed_region_checker_path": str(prescribed_region_checker),
        "prescribed_region_checker_sha256": _sha(
            prescribed_region_checker.read_bytes()
        ),
        "cargo_path": str(cargo),
        "cargo_sha256": _sha(cargo.read_bytes()),
        "rustc_path": str(rustc),
        "rustc_sha256": _sha(rustc.read_bytes()),
        "toolchain_root": str(toolchain),
        "charon_toolchain_root": str(toolchain),
        "charon_toolchain_tree_sha256": RUNNER.readonly_closure_digest(toolchain),
        "charon_path": str(charon),
        "charon_sha256": _sha(charon.read_bytes()),
        "charon_source_root": str(charon_root),
        "charon_source_tree_sha256": RUNNER.git_checkout_digest(charon_root),
        "aeneas_path": str(aeneas),
        "aeneas_sha256": _sha(aeneas.read_bytes()),
        "aeneas_source_root": str(aeneas_root),
        "aeneas_source_tree_sha256": RUNNER.git_checkout_digest(aeneas_root),
        "dependency_cache_path": "",
        "dependency_cache_tree_sha256": _sha(b""),
        "opam_switch_prefix": str(opam_switch_prefix),
        "opam_switch_prefix_tree_sha256": RUNNER.readonly_closure_digest(
            opam_switch_prefix
        ),
        "opam_switch_environment": opam_switch_environment,
        "output_root": str(output),
        "output_root_binding_sha256": RUNNER.output_root_binding(output),
        "target_triple": "x86_64-unknown-linux-gnu",
        "extraction_profile": "dev",
        "opam_switch": "fixture",
        "closed_environment": {"LANG": "C.UTF-8"},
        "run_id": "run-1",
        "limits": {
            "timeout_seconds": 10,
            "cpu_seconds": 8,
            "address_space_bytes": 2_147_483_648,
            "file_size_bytes": 67_108_864,
            "process_count": 64,
            "stdout_bytes": 4096,
            "stderr_bytes": 4096,
        },
    }


def test_no_cargo_module_is_structured_standalone_failure(tmp_path: Path) -> None:
    request = _fixture(tmp_path)
    candidate = Path(request["candidate_root"])
    (candidate / "Cargo.toml").unlink()
    request["candidate_manifest"] = RUNNER.source_tree_manifest(candidate)
    request["candidate_tree_sha256"] = request["candidate_manifest"]["source_tree_sha256"]
    output = RUNNER.run(request, sandbox_enabled=False)
    assert output["status"] == "failed"
    assert output["first_failing_stage"] == "standalone_package"
    assert [stage["name"] for stage in output["stages"]] == ["standalone_package"]
    # Path-free: the same diagnostic must recur verbatim in a later ablation
    # check whose check id (and hence sealed-copy path) differs.
    assert output["reason"] == "standalone Cargo.toml is unavailable: No such file or directory"
    assert "/" not in output["reason"]


def test_build_script_out_dir_candidate_is_refused_as_non_standalone(
    tmp_path: Path,
) -> None:
    request = _fixture(tmp_path)
    candidate = Path(request["candidate_root"])
    (candidate / "Cargo.toml").write_text(
        '[package]\nname="fixture"\nversion="0.1.0"\nedition="2021"\nbuild="build.rs"\n',
        encoding="utf-8",
    )
    (candidate / "build.rs").write_text(
        'fn main() { std::fs::write(std::env::var("OUT_DIR").unwrap() + "/x.rs", "pub fn x() {} ").unwrap(); }\n',
        encoding="utf-8",
    )
    (candidate / "src/lib.rs").write_text(
        'include!(concat!(env!("OUT_DIR"), "/x.rs"));\n', encoding="utf-8"
    )
    request["candidate_manifest"] = RUNNER.source_tree_manifest(candidate)
    request["candidate_tree_sha256"] = request["candidate_manifest"]["source_tree_sha256"]
    output = RUNNER.run(request, sandbox_enabled=False)
    assert output["first_failing_stage"] == "standalone_package"
    assert "build script" in output["reason"]


def test_dep_info_rejects_source_outside_sealed_copy(tmp_path: Path) -> None:
    request = _fixture(tmp_path)
    outside = tmp_path / "outside.rs"
    outside.write_text("pub const SECRET: u8 = 7;\n", encoding="utf-8")
    cargo = Path(request["cargo_path"])
    _program(
        cargo,
        "#!/usr/bin/python3\n"
        "import pathlib,sys\n"
        "args=sys.argv[1:]; target=pathlib.Path(args[args.index('--target-dir')+1])\n"
        "dep=target/'debug/deps/fixture.d'; dep.parent.mkdir(parents=True,exist_ok=True)\n"
        f"dep.write_text(f'{{dep}}: {{pathlib.Path.cwd()/\"src/lib.rs\"}} {outside}\\n')\n",
    )
    request["cargo_sha256"] = _sha(cargo.read_bytes())
    request["charon_toolchain_tree_sha256"] = RUNNER.readonly_closure_digest(
        Path(request["charon_toolchain_root"])
    )
    output = RUNNER.run(request, sandbox_enabled=False)
    assert output["first_failing_stage"] == "cargo_dep_info_closure"
    assert str(outside.resolve()) in output["reason"]


def test_fake_tools_take_tiny_crate_to_candidate_success(tmp_path: Path) -> None:
    original_home = os.environ.get("HOME")
    request = _fixture(tmp_path)
    output = RUNNER.run(request, sandbox_enabled=False)
    assert os.environ.get("HOME") == original_home
    assert output["status"] == "passed", output
    names = [stage["name"] for stage in output["stages"]]
    assert names[:2] == ["standalone_package", "cargo_check"]
    assert {"rust_identity", "charon", "cargo_metadata_cfg", "aeneas", "generated_model_split"} <= set(names)
    assert output["llbc_root_sha256"] == _sha(b"llbc")
    # A model whose every declaration has a body reads as an empty list, and
    # says which file it read; it is never reported as unread.
    assert output["model_bodiless_declarations"] == {
        "status": "read",
        "read_from": "generated.lean",
        "detail": "",
        "declarations": [],
    }
    assert output["generated_lean_root_sha256"]
    assert output["tablet_root_sha256"]
    assert output["source_facts"]["compile_initial_tree_sha256"] == output["candidate_tree_sha256"]
    assert output["source_facts"]["extraction_initial_tree_sha256"] == output["candidate_tree_sha256"]
    extraction_stage = next(stage for stage in output["stages"] if stage["name"] == "charon")
    extraction_root = Path(request["output_root"]) / request["run_id"] / "extraction-writable"
    expected_environment = dict(request["closed_environment"])
    expected_environment.update(request["opam_switch_environment"])
    expected_environment.update(
        {
            "PATH": os.pathsep.join(
                (
                    str(Path(request["charon_toolchain_root"]) / "bin"),
                    str(Path(request["charon_path"]).parent),
                    str(Path(request["aeneas_path"]).parent),
                    str(Path(request["opam_switch_prefix"]) / "bin"),
                    "/usr/bin",
                    "/bin",
                )
            ),
            "HOME": str(extraction_root),
            "CARGO_HOME": str(extraction_root / "cargo-home"),
            "TMPDIR": str(extraction_root / "tmp"),
            "CHARON_TOOLCHAIN_IS_IN_PATH": "1",
            "LD_LIBRARY_PATH": str(Path(request["charon_toolchain_root"]) / "lib"),
            "CARGO_NET_OFFLINE": "true",
            "CARGO_TARGET_DIR": str(extraction_root / "target/extract"),
            "CARGO_BUILD_TARGET": request["target_triple"],
            "RUSTUP_TOOLCHAIN": "nightly-2026-06-01",
        }
    )
    assert extraction_stage["environment"] == dict(sorted(expected_environment.items()))
    assert "OPAMROOT" not in extraction_stage["environment"]
    assert request["filespec_checker_path"] in output["sandbox_mounts"]["read_only"]
    assert request["prescribed_region_checker_path"] in output["sandbox_mounts"]["read_only"]
    for stage in output["stages"]:
        for stream in ("stdout", "stderr"):
            raw = __import__("base64").b64decode(stage[f"{stream}_base64"])
            assert len(raw) == stage[f"{stream}_byte_length"]
            assert _sha(raw) == stage[f"{stream}_sha256"]


@pytest.mark.parametrize(
    ("fixture_failure", "expected_stage"),
    [
        ("rust_identity", "rust_identity"),
        ("charon", "charon"),
        ("cargo_metadata_cfg", "cargo_metadata_cfg"),
        ("cargo_cfg", "cargo_metadata_cfg"),
        ("aeneas", "aeneas"),
        ("generated_model_split", "generated_model_split"),
    ],
)
def test_fake_tool_failure_is_attributed_to_exact_stage(
    tmp_path: Path, fixture_failure: str, expected_stage: str
) -> None:
    output = RUNNER.run(
        _fixture(tmp_path, failure_stage=fixture_failure), sandbox_enabled=False
    )
    assert output["status"] == "failed"
    assert output["first_failing_stage"] == expected_stage
    assert any(
        stage["name"] == expected_stage and stage["status"] == "failed"
        for stage in output["stages"]
    )
    # No model was produced, so the scan says so rather than reporting a clean
    # model with nothing suppressed.
    assert output["model_bodiless_declarations"]["status"] == "not_extracted"
    assert output["model_bodiless_declarations"]["declarations"] == []


def test_fake_filespec_failure_is_attributed_to_gate_stage(tmp_path: Path) -> None:
    request = _fixture(tmp_path, failure_stage="filespec_region_gates")
    request["check_kind"] = "ratification_replay"
    output = RUNNER.run(request, sandbox_enabled=False)
    assert output["status"] == "failed"
    assert output["first_failing_stage"] == "filespec_region_gates"
    assert output["stages"][-1]["name"] == "filespec_region_gates"
    assert output["stages"][-1]["command"]
    assert output["stages"][-1]["tool"] == "kernel-scan-tablet-filespec"


def test_a_model_that_skips_a_body_is_a_checker_fact(tmp_path: Path) -> None:
    """The suppressed computation is read out of the model the tools emitted."""

    output = RUNNER.run(_fixture(tmp_path, bodiless=True), sandbox_enabled=False)
    assert output["status"] == "passed", output
    scan = output["model_bodiless_declarations"]
    assert scan["status"] == "read"
    assert scan["read_from"] == "generated.lean"
    assert scan["detail"] == ""
    assert scan["declarations"] == [
        {
            "name": "fixture.helper",
            "kind": "axiom",
            "item_path": "fixture::helper",
        }
    ]


def test_checker_limits_have_fixed_fail_closed_maxima(tmp_path: Path) -> None:
    request = _fixture(tmp_path)
    request["limits"]["stdout_bytes"] = RUNNER._LIMIT_MAXIMUMS["stdout_bytes"] + 1
    with pytest.raises(RUNNER.CheckerFailure, match="checker maximum"):
        RUNNER.run(request, sandbox_enabled=False)


def test_fake_tools_take_tiny_crate_to_final_a_b_success(tmp_path: Path) -> None:
    request = _fixture(tmp_path)
    request["check_kind"] = "ratification_replay"
    output = RUNNER.run(request, sandbox_enabled=False)
    assert output["status"] == "passed", output
    assert len(output["a_b_generated_lean_roots"]) == 2
    assert len(set(output["a_b_generated_lean_roots"])) == 1
    assert len(output["a_b_tablet_roots"]) == 2
    assert len(set(output["a_b_tablet_roots"])) == 1
    gate_stages = [
        stage for stage in output["stages"] if stage["name"] == "filespec_region_gates"
    ]
    assert gate_stages
    assert not any(stage["tool"] == "cargo-build-kernel-example" for stage in gate_stages)
    evidence = json.loads(
        (
            Path(request["output_root"])
            / request["run_id"]
            / "extraction-writable/extracted/extraction-evidence/COMMAND.json"
        ).read_bytes()
    )
    assert evidence["kernel_gate_checkers"] == [
        {
            "tool": "kernel-scan-tablet-filespec",
            "path": request["filespec_checker_path"],
            "sha256": request["filespec_checker_sha256"],
        },
        {
            "tool": "kernel-print-prescribed-region",
            "path": request["prescribed_region_checker_path"],
            "sha256": request["prescribed_region_checker_sha256"],
        },
    ]


def test_no_network_request_forces_fake_cargo_offline(tmp_path: Path) -> None:
    """The fixture Cargo asserts offline before any registry path is reachable."""

    output = RUNNER.run(_fixture(tmp_path), sandbox_enabled=False)
    assert output["status"] == "passed", output
    cargo_stages = [
        stage
        for stage in output["stages"]
        if stage["tool"] in {"cargo", "cargo-metadata", "cargo-rustc-cfg"}
    ]
    assert cargo_stages
    assert all(stage["environment"]["CARGO_NET_OFFLINE"] == "true" for stage in cargo_stages)


def test_timeout_and_truncation_are_checker_failures(tmp_path: Path) -> None:
    timeout_request = _fixture(tmp_path / "timeout", cargo_mode="timeout")
    timeout_request["limits"]["timeout_seconds"] = 1
    timeout = RUNNER.run(timeout_request, sandbox_enabled=False)
    assert timeout["first_failing_stage"] == "cargo_check"
    assert timeout["stages"][-1]["timed_out"] is True

    truncate_request = _fixture(tmp_path / "truncate", cargo_mode="truncate")
    truncate_request["limits"]["stdout_bytes"] = 32
    truncated = RUNNER.run(truncate_request, sandbox_enabled=False)
    assert truncated["first_failing_stage"] == "cargo_check"
    assert truncated["stages"][-1]["stdout_truncated"] is True


def test_request_and_tool_drift_are_rejected(tmp_path: Path) -> None:
    request = _fixture(tmp_path)
    request["unknown"] = True
    with pytest.raises(RUNNER.CheckerFailure, match="missing/unknown"):
        RUNNER.run(request, sandbox_enabled=False)


def test_request_refuses_opam_switch_environment_escape(tmp_path: Path) -> None:
    request = _fixture(tmp_path)
    outside = tmp_path / "outside-opam-path"
    outside.mkdir()
    request["opam_switch_environment"]["OCAML_TOPLEVEL_PATH"] = str(outside)
    with pytest.raises(RUNNER.CheckerFailure, match="escapes the mounted switch prefix"):
        RUNNER.run(request, sandbox_enabled=False)


def test_request_refuses_opam_switch_prefix_closure_drift(tmp_path: Path) -> None:
    request = _fixture(tmp_path)
    (Path(request["opam_switch_prefix"]) / "identity").write_text(
        "drifted\n", encoding="utf-8"
    )
    with pytest.raises(RUNNER.CheckerFailure, match="read-only closure drifted"):
        RUNNER.run(request, sandbox_enabled=False)


@pytest.mark.parametrize(
    ("path_field", "digest_field"),
    [
        ("filespec_checker_path", "filespec_checker_sha256"),
        ("prescribed_region_checker_path", "prescribed_region_checker_sha256"),
    ],
)
def test_request_refuses_kernel_gate_checker_digest_drift(
    tmp_path: Path, path_field: str, digest_field: str
) -> None:
    request = _fixture(tmp_path)
    Path(request[path_field]).write_bytes(b"#!/bin/sh\nexit 9\n")
    with pytest.raises(RUNNER.CheckerFailure, match="differs from its pinned digest"):
        RUNNER.run(request, sandbox_enabled=False)


def test_output_root_cannot_overlap_a_pinned_input(tmp_path: Path) -> None:
    request = _fixture(tmp_path)
    request["output_root"] = str(tmp_path)
    request["output_root_binding_sha256"] = RUNNER.output_root_binding(tmp_path)
    with pytest.raises(RUNNER.CheckerFailure, match="overlaps"):
        RUNNER.run(request, sandbox_enabled=False)


def test_fresh_compile_and_extraction_copies_are_root_verified(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    request = _fixture(tmp_path)
    real_copytree = RUNNER.shutil.copytree

    def drifting_copytree(source: Path, destination: Path, *args, **kwargs):
        copied = real_copytree(source, destination, *args, **kwargs)
        if Path(destination).parent.name == "compile-writable":
            (Path(destination) / "unlogged-byte").write_bytes(b"x")
        return copied

    monkeypatch.setattr(RUNNER.shutil, "copytree", drifting_copytree)
    with pytest.raises(RUNNER.CheckerFailure, match="fresh checker source copy"):
        RUNNER.run(request, sandbox_enabled=False)

    request = _fixture(tmp_path / "drift")
    Path(request["cargo_path"]).write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    with pytest.raises(RUNNER.CheckerFailure, match="pinned digest"):
        RUNNER.run(request, sandbox_enabled=False)


def test_compile_leg_cannot_contaminate_extraction_copy(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    request = _fixture(tmp_path)
    real_run = RUNNER._run_command

    def contaminating_run(**kwargs):
        outcome = real_run(**kwargs)
        if kwargs["name"] == "cargo_check":
            extraction_source = (
                Path(kwargs["writable"]).parent / "extraction-writable" / "source"
            )
            (extraction_source / "unlogged-byte").write_bytes(b"x")
        return outcome

    monkeypatch.setattr(RUNNER, "_run_command", contaminating_run)
    with pytest.raises(RUNNER.CheckerFailure, match="changed before extraction"):
        RUNNER.run(request, sandbox_enabled=False)


def test_source_adaptation_sandbox_role_is_closed_and_worker_does_not_gain_tools(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    request = _fixture(tmp_path)
    candidate = Path(request["candidate_root"])
    writable = Path(request["output_root"])
    tool = Path(request["charon_path"])
    monkeypatch.setenv("TRELLIS_SANDBOX_EXTRA_RO_PATHS", str(tool.parent.parent))
    monkeypatch.setenv("PATH", "/usr/bin:/bin")
    monkeypatch.setenv("LD_LIBRARY_PATH", "/sealed/charon/lib")
    monkeypatch.setenv("CHARON_TOOLCHAIN_IS_IN_PATH", "1")
    monkeypatch.setenv("CARGO_NET_OFFLINE", "true")
    wrapped = RUNNER.wrap_command(
        ["/bin/true"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=candidate,
        burst_home=writable,
        role="source_adaptation_checker",
    )
    assert "--unshare-net" in wrapped and "--clearenv" in wrapped
    for key, value in (
        ("LD_LIBRARY_PATH", "/sealed/charon/lib"),
        ("CHARON_TOOLCHAIN_IS_IN_PATH", "1"),
        ("CARGO_NET_OFFLINE", "true"),
    ):
        index = wrapped.index(key)
        assert wrapped[index - 1 : index + 2] == ["--setenv", key, value]
    assert ["--ro-bind", str(tool.parent.parent.resolve()), str(tool.parent.parent.resolve())] == wrapped[
        wrapped.index(str(tool.parent.parent.resolve())) - 1:wrapped.index(str(tool.parent.parent.resolve())) + 2
    ]
    bind_sources = [
        wrapped[index + 1]
        for index, value in enumerate(wrapped)
        if value in {"--bind", "--ro-bind"}
    ]
    assert not any(
        "/.codex" in item or "/.claude" in item or "/.gemini" in item
        for item in bind_sources
    )

    monkeypatch.delenv("TRELLIS_SANDBOX_EXTRA_RO_PATHS")
    worker = RUNNER.wrap_command(
        ["/bin/true"], sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=candidate, burst_home=writable, role="worker",
    )
    assert str(tool.parent.parent.resolve()) not in worker


def test_host_required_real_bwrap_checker_closure_reaches_clean_a_b_replay(
    tmp_path: Path,
) -> None:
    """Host-only proof that the genesis closure really runs Charon and Aeneas."""

    probe = subprocess.run(
        ["bwrap", "--ro-bind", "/", "/", "--", "/bin/true"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    if probe.returncode != 0:
        pytest.fail("host-required bubblewrap namespace is unavailable (tests are nested)")
    def required_host_path(variable: str, *, executable: bool = False) -> Path:
        configured = os.environ.get(variable, "").strip()
        if not configured:
            pytest.fail(f"host-required Phase-0 tool/artifact path must be configured: {variable}")
        try:
            value = Path(configured).resolve(strict=True)
        except OSError as error:
            pytest.fail(f"host-required Phase-0 path is unavailable: {variable}: {error}")
        if executable and (not value.is_file() or not os.access(value, os.X_OK)):
            pytest.fail(f"host-required Phase-0 executable is unavailable: {variable}: {value}")
        if not executable and not value.is_dir():
            pytest.fail(f"host-required Phase-0 directory is unavailable: {variable}: {value}")
        return value

    charon = required_host_path("TRELLIS_TEST_CHARON", executable=True)
    aeneas = required_host_path("TRELLIS_TEST_AENEAS", executable=True)
    opam_switch_prefix = required_host_path("TRELLIS_TEST_OPAM_SWITCH_PREFIX")
    filespec_checker = required_host_path("TRELLIS_TEST_FILESPEC_CHECKER", executable=True)
    prescribed_region_checker = required_host_path("TRELLIS_TEST_PRESCRIBED_REGION_CHECKER", executable=True)
    request = _fixture(tmp_path)
    candidate = Path(request["candidate_root"])
    shutil.rmtree(candidate)
    shutil.copytree(ROOT / "kernel/tests/fixtures/pv_generic_source", candidate)
    (candidate / "GOAL.md").unlink()
    manifest = RUNNER.source_tree_manifest(candidate)
    request["candidate_manifest"] = manifest
    request["candidate_tree_sha256"] = manifest["source_tree_sha256"]
    goal = ROOT / "kernel/tests/fixtures/pv_generic_source/GOAL.md"
    request["goal_path"] = str(goal)
    request["goal_sha256"] = _sha(goal.read_bytes())
    cargo = Path(subprocess.check_output(["rustup", "which", "cargo"], text=True).strip()).resolve()
    rustc = Path(subprocess.check_output(["rustup", "which", "rustc"], text=True).strip()).resolve()
    # Host validation consumes explicitly built optimized gate artifacts. A
    # test must never quietly compile a second kernel with a different profile.
    primary_toolchain = cargo.parent.parent
    charon_toolchain = Path(
        subprocess.check_output([str(charon), "toolchain-path"], text=True).strip()
    ).resolve(strict=True)
    request.update(
        {
            "cargo_path": str(cargo),
            "cargo_sha256": _sha(cargo.read_bytes()),
            "rustc_path": str(rustc),
            "rustc_sha256": _sha(rustc.read_bytes()),
            "filespec_checker_path": str(filespec_checker),
            "filespec_checker_sha256": _sha(filespec_checker.read_bytes()),
            "prescribed_region_checker_path": str(prescribed_region_checker),
            "prescribed_region_checker_sha256": _sha(
                prescribed_region_checker.read_bytes()
            ),
            "toolchain_root": str(primary_toolchain),
            "charon_toolchain_root": str(charon_toolchain),
            "charon_toolchain_tree_sha256": RUNNER.readonly_closure_digest(charon_toolchain),
            "charon_path": str(charon),
            "charon_sha256": _sha(charon.read_bytes()),
            "charon_source_root": str(charon.parents[1]),
            "charon_source_tree_sha256": RUNNER.git_checkout_digest(charon.parents[1]),
            "aeneas_path": str(aeneas),
            "aeneas_sha256": _sha(aeneas.read_bytes()),
            "aeneas_source_root": str(aeneas.parents[1]),
            "aeneas_source_tree_sha256": RUNNER.git_checkout_digest(aeneas.parents[1]),
            "opam_switch_prefix": str(opam_switch_prefix),
            "opam_switch_prefix_tree_sha256": RUNNER.readonly_closure_digest(
                opam_switch_prefix
            ),
            "opam_switch_environment": {
                "OPAM_SWITCH_PREFIX": str(opam_switch_prefix),
                "OPAMSWITCH": "5.3.0",
                "CAML_LD_LIBRARY_PATH": os.pathsep.join(
                    str(opam_switch_prefix / relative)
                    for relative in (
                        "lib/stublibs",
                        "lib/ocaml/stublibs",
                        "lib/ocaml",
                    )
                ),
                "OCAML_TOPLEVEL_PATH": str(opam_switch_prefix / "lib/toplevel"),
                "OCAMLTOP_INCLUDE_PATH": str(opam_switch_prefix / "lib/toplevel"),
            },
            "opam_switch": "5.3.0",
            "check_kind": "ratification_replay",
            "run_id": "real-host-a-b",
        }
    )
    # Real Charon + Aeneas plus the two-run gate on the fixture crate take
    # minutes, not the fake-tool fixture's seconds; size the limits for the
    # real toolchain (within the runner's maximums).
    request["limits"] = {
        "timeout_seconds": 1800,
        "cpu_seconds": 1500,
        "address_space_bytes": 8_589_934_592,
        "file_size_bytes": 1_073_741_824,
        "process_count": 4096,
        "stdout_bytes": 1024 * 1024,
        "stderr_bytes": 1024 * 1024,
    }
    output = RUNNER.run(request, sandbox_enabled=True)
    assert output["status"] == "passed", output
    names = [stage["name"] for stage in output["stages"]]
    assert {
        "standalone_package", "cargo_check", "cargo_dep_info_closure",
        "rust_identity", "charon", "aeneas", "generated_model_split",
        "second_clean_extraction", "filespec_region_gates",
    } <= set(names)
    assert len(set(output["a_b_generated_lean_roots"])) == 1
    assert len(set(output["a_b_tablet_roots"])) == 1


def test_missing_cargo_lock_is_a_named_standalone_failure(tmp_path: Path) -> None:
    request = _fixture(tmp_path)
    candidate = Path(request["candidate_root"])
    (candidate / "Cargo.lock").unlink()
    request["candidate_manifest"] = RUNNER.source_tree_manifest(candidate)
    request["candidate_tree_sha256"] = request["candidate_manifest"]["source_tree_sha256"]
    output = RUNNER.run(request, sandbox_enabled=False)
    assert output["status"] == "failed"
    assert output["first_failing_stage"] == "standalone_package"
    assert "Cargo.lock" in output["reason"]
    assert "read-only" in output["reason"]
