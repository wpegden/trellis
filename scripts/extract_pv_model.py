#!/usr/bin/env python3
"""Extract a Rust crate to a byte-pinned Lean ExtractionModel + FILESPEC-split it.

The operational layer that PRODUCES what the kernel's PV Phase-1/2 machinery
CONSUMES (the `pv_tablet` config block, byte-pinned via
`challenge_conformance_errors` + `extraction_chain_errors`).

Pipeline (the verified toolchain — see /path/to/trellis/pv_spike/toolchain.manifest):

    charon cargo --preset=aeneas --dest-file <out>/m.llbc      # Rust -> LLBC
    aeneas -backend lean -dest <out> <out>/m.llbc              # LLBC -> single Lean

then a deterministic FILESPEC split of the single generated Lean file:

  * the shared header (`-- AUTOMATICALLY GENERATED ...`, `import Aeneas`,
    `open ...`, the fixed `set_option`s) -> a `Tablet/Preamble.lean` node
    (the `namespace <crate>` line is dropped from Preamble; each node file
    re-emits it in its OWN header — see below);
  * each top-level `def` -> its own `Tablet/<Node>.lean` carrying exactly one
    `-- [TABLET NODE: <Name>]` marker and exactly one `-- BODY` marker, with
    the `-- BODY` placed right after the line ending in the principal
    declaration's `:=` body delimiter (so the kernel `validate_filespec`
    body-marker rule holds; aeneas's `:= do` is reflowed to `:=` \n `do`,
    which is whitespace-identical Lean — verified to typecheck);
  * a loop is co-generated as a TRIO (`f_loop.body` `@[rust_loop_body]`,
    `f_loop` `@[rust_loop]`, `@[reducible] def f`). The trio is ONE node named
    `f` (sign-off 1): inseparable, all three decls in one file's pinned region.

Each node file gets a deterministic HEADER, placed ABOVE the
`-- [TABLET NODE: <Name>]` marker (OUTSIDE the byte-pinned region):

  * `import Tablet.Preamble` + `import Tablet.<Sibling>` for every sibling node
    this node's declaration references. The sibling set is computed
    DETERMINISTICALLY from the tablet DAG: scan the decl text for identifier
    tokens that are declared names owned by another node (principal OR a
    co-generated member — a projector, simp lemma, or an inner `def` of a
    `mutual`/loop-trio block). These `import Tablet.X` lines are also exactly
    what the kernel re-extracts as the node `deps` (Phase-2 `reverse_dep_closure`).
  * `open Aeneas Aeneas.Std Result ControlFlow Error` — Lean's `open` is
    FILE-LOCAL, so the Preamble's `open` does NOT cross `import Tablet.Preamble`;
    each node must re-open for `Std.U32`/`Result`/`ok`/`lift` to resolve.
  * `namespace <crate>` with NO closing `end`. A namespace auto-closes at end of
    file, so the pinned region (decl signature-through-body) stays the bare
    declaration — never `end <crate>`. The re-opened namespace is what lets a
    cross-file reference to a sibling (declared as `<crate>.<Name>`) resolve.

The decl + `-- BODY` region (the `prescribed_region(include_body=true)`) is the
pinned `lean`; the header lives outside it (soundness: the byte-pin is the DECL).

The emitted `pv_tablet` block records the compiler's full `rustc -vV`, target
triple and pointer width, resolved Cargo features and lockfile digest, profile,
overflow/panic settings, effective cfg values, and the Charon/Aeneas identities
and source revisions.  The kernel computes one canonical toolchain fingerprint
from that evidence.  Each model also carries the fingerprint, problem ID,
source file, and a complete regular-file crate-tree `source_sha256` (sign-off
2).  The tree hash commits normalized relative paths, byte lengths, and raw
file digests; symlinks and special files are rejected.  Facts which genuinely
cannot be captured are omitted as values and listed in `not_recorded` with a
reason; empty strings are never emitted as evidence.

Alongside, the extractor DETERMINISTICALLY EMITS a Tier-B TCB-disclosure manifest
(`<out>/tcb_manifest.json`, also `--emit-tcb-manifest <path>`): routed boundary
axioms with their classification and Lean type. Boundary names keep the existing
manifest form; setup writes the exact `APPROVED_AXIOMS.json` allowlist from this
manifest.

GATE mode (--gate) proves, against the REAL kernel logic (the `scan_tablet_filespec`
and `print_prescribed_region` example binaries):
  (a) two back-to-back extractions from a clean target -> byte-identical Model
      files (automated A/B sha diff);
  (b) every Model file passes `validate_filespec` (one TABLET-NODE + one -- BODY,
      body delimiter present);
  (c) each emitted `lean` text byte-equals the file's `prescribed_region`.

Usage:
  extract_pv_model.py --crate <dir> --out <dir> [--node-prefix <P>]
                      [--charon-bin <path>] [--aeneas-bin <path>]
                      [--opam-switch 5.3.0] [--lean-version 4.30.0-rc2]
                      [--kernel-manifest <kernel/Cargo.toml>]
                      [--cargo-target-dir <dir>] [--work-root <dir>]
                      [--gate] [--emit-tcb-manifest <path>]

Writes ONLY under --out (and a private scratch under --work-root, reaped). Never
touches the source crate's own `target/`: the crate is copied into scratch and
extraction runs there.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Set, Tuple

try:
    from scripts.pv_source_tree import (
        SOURCE_TREE_MANIFEST_SCHEMA,
        SourceTreeError,
        source_tree_digest,
        source_tree_manifest,
        source_tree_root_from_entries,
    )
except ModuleNotFoundError:  # direct script execution
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from pv_source_tree import (  # type: ignore[no-redef]
        SOURCE_TREE_MANIFEST_SCHEMA,
        SourceTreeError,
        source_tree_digest,
        source_tree_manifest,
        source_tree_root_from_entries,
    )

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from trellis.pv_lean_resources import generated_elaboration_options, prepare_generated_lean
from trellis.pv_extraction_config import extraction_arguments
from trellis.pv_execution import run_compute
from trellis.lean_declarations import declaration_heads, mask_comments

EXTRACTOR_SCRIPT_PATH = Path(__file__).resolve()
EXTRACTOR_SCRIPT_SHA256 = hashlib.sha256(EXTRACTOR_SCRIPT_PATH.read_bytes()).hexdigest()

# ── defaults: the verified shared install (sign-off 3 — reference, don't vendor) ──
DEFAULT_TOOLS = Path("/path/to/trellis/pv_spike/tools")
DEFAULT_CHARON_BIN = DEFAULT_TOOLS / "charon" / "bin" / "charon"
DEFAULT_AENEAS_BIN = DEFAULT_TOOLS / "aeneas" / "bin" / "aeneas"
DEFAULT_OPAM_SWITCH = "5.3.0"
DEFAULT_LEAN_VERSION = "4.30.0-rc2"
PINNED_RUST_TOOLCHAIN = "nightly-2026-06-01"
# The pinned extractor versions (toolchain.manifest). These populate
# `extractor_stack` + `extraction_toolchain`; the kernel hashes them into the
# `extractor_toolchain_sha256` the acceptance gate + reconcile compare on.
DEFAULT_CHARON_VERSION = "0.1.216"
DEFAULT_AENEAS_VERSION = "fa699427"

PREAMBLE_NODE = "Preamble"

# PV under-model assumptions: the Preamble-tier node where workers can stage
# marked Lean/NL blocks for later ratification.
ASSUMPTIONS_NODE = "Assumptions"


def _empty_assumptions_lean(validity_definitions: Optional[List[dict]] = None) -> str:
    """The seeded `Tablet/Assumptions.lean` scaffold.

    `import Tablet.Preamble` lets later worker-authored assumptions reference
    model types. The seed itself contains no definitions or assumptions.
    """
    definitions = "".join(item["definition_source_utf8"].rstrip("\n") + "\n\n"
                          for item in (validity_definitions or []))
    return (
        "import Tablet.Preamble\n"
        "open Aeneas Aeneas.Std Result ControlFlow Error\n\n"
        + definitions
        + "-- [PV UNDER-MODEL ASSUMPTIONS]\n"
        "-- Worker-authored staged assumptions live between markers:\n"
        "--   -- BEGIN UNDERMODEL ASSUMPTION <id>\n"
        "--   axiom <name> : <type>\n"
        "--   -- END UNDERMODEL ASSUMPTION <id>\n"
    )


def _empty_assumptions_tex() -> str:
    """The empty paired NL scaffold for `Tablet/Assumptions.tex`.

    Kept in sync with
    `kernel/src/assumptions_registry.rs::empty_assumptions_tex_scaffold`.
    """
    return (
        "% [PV UNDER-MODEL ASSUMPTIONS]\n"
        "% Worker-authored staged assumptions live between markers:\n"
        "%   % BEGIN UNDERMODEL ASSUMPTION <id>\n"
        "%   \\begin{definition}\n"
        "%   ...\n"
        "%   \\end{definition}\n"
        "%   % END UNDERMODEL ASSUMPTION <id>\n"
    )


# Free-preamble `maxRecDepth` bump for big `Array.make N#usize [...]` const tables
# (see node-header emission). Threshold sits above the largest table that compiles
# under the Preamble's 2048 default (LSHIFT_TABLE = 65) and below the smallest that
# does not (POWER_OF_FIVE_128 = 651); 65536 covers the largest table (1308).
_BIG_ARRAY_RE = re.compile(r"Array\.make\s+(\d+)#usize")
_BIG_ARRAY_RECDEPTH_THRESHOLD = 256
_BIG_ARRAY_RECDEPTH = 65536
def load_model_boundary_contract(path: Optional[Path]) -> dict:
    """Load the seed-authored boundary input used by extraction.

    The input may be either the complete campaign trust plan or its
    `model_boundary_contract` object. No reviewed definition is built into
    this extractor.
    """
    if path is None:
        return {
            "schema": "trellis-campaign-model-boundary-contract/v1",
            "model_semantics": "trellis-aeneas-boundary-definitions/v1",
            "boundary_definitions": [],
            "validity_definitions": [],
        }
    raw = path.read_bytes()
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise SystemExit("model-boundary input must be a JSON object")
    if "model_boundary_contract" in value:
        value = value["model_boundary_contract"]
    if not isinstance(value, dict):
        raise SystemExit("model_boundary_contract must be a JSON object")
    if value.get("schema") != "trellis-campaign-model-boundary-contract/v1":
        raise SystemExit("unsupported model-boundary contract schema")
    boundaries = value.get("boundary_definitions")
    validities = value.get("validity_definitions")
    if not isinstance(boundaries, list) or not isinstance(validities, list):
        raise SystemExit(
            "model-boundary contract needs boundary_definitions and validity_definitions"
        )
    prior = None
    required_boundary = {
        "name",
        "expected_extracted_lean_type_utf8",
        "reviewed_definition_source_utf8",
        "reviewed_definition_lean_type_utf8",
        "rust_symbol_utf8",
        "approved_rust_monomorphizations",
    }
    for item in boundaries:
        if not isinstance(item, dict) or not required_boundary.issubset(item):
            raise SystemExit("model-boundary definition is incomplete")
        name = item["name"]
        if (
            not isinstance(name, str)
            or not name
            or (prior is not None and prior.encode() >= name.encode())
            or not isinstance(item["reviewed_definition_source_utf8"], str)
            or not item["reviewed_definition_source_utf8"].endswith("\n")
            or not item["reviewed_definition_source_utf8"].startswith(
                f"def {name}"
            )
            or not isinstance(item["approved_rust_monomorphizations"], list)
            or not item["approved_rust_monomorphizations"]
        ):
            raise SystemExit(
                "model-boundary definitions must be exact, nonempty, and name sorted"
            )
        for field in (
            "expected_extracted_lean_type_utf8",
            "reviewed_definition_lean_type_utf8",
            "rust_symbol_utf8",
        ):
            if not isinstance(item[field], str) or not item[field]:
                raise SystemExit(f"model-boundary definition has invalid {field}")
        prior = name
    prior = None
    for item in validities:
        if not isinstance(item, dict) or set(
            (
                "name",
                "definition_source_utf8",
                "definition_lean_type_utf8",
            )
        ) - set(item):
            raise SystemExit("model validity definition is incomplete")
        name = item["name"]
        if (
            not isinstance(name, str)
            or not name
            or (prior is not None and prior.encode() >= name.encode())
            or not isinstance(item["definition_source_utf8"], str)
            or not item["definition_source_utf8"].endswith("\n")
            or not item["definition_source_utf8"].startswith(f"def {name}")
            or not isinstance(item["definition_lean_type_utf8"], str)
            or not item["definition_lean_type_utf8"]
        ):
            raise SystemExit(
                "validity definitions must be exact, nonempty, and name sorted"
            )
        prior = name
    return value


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


# ───────────────────────────── env / pipeline ──────────────────────────────


def _closed_host_tool_env() -> Dict[str, str]:
    """A packageable host-tool environment with no ambient credentials."""
    env = {
        key: os.environ[key]
        for key in (
            "HOME",
            "USER",
            "LOGNAME",
            "SHELL",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "RUSTUP_HOME",
            "CARGO_HOME",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            # Charon's launcher runs its driver through rustup unless this
            # flag says the pinned toolchain's bin is already first on PATH,
            # which is how the phase-0 checker sandbox supplies it.
            "CHARON_TOOLCHAIN_IS_IN_PATH",
            "LD_LIBRARY_PATH",
        )
        if key in os.environ
    }
    env.setdefault("LANG", "C.UTF-8")
    env["PATH"] = os.environ.get("PATH", "/usr/local/bin:/usr/bin:/bin")
    # The extraction closure has no network.  Keep Cargo's own network gate
    # closed in addition to spelling --offline on every Cargo command, so an
    # accidental nested invocation fails immediately instead of retrying DNS.
    env["CARGO_NET_OFFLINE"] = "true"
    # The extracted Rust program and the kernel evidence checkers must not be
    # compiled under a floating host default.  This date-pinned nightly is also
    # Charon's reviewed compiler basis.
    env["RUSTUP_TOOLCHAIN"] = PINNED_RUST_TOOLCHAIN
    return env


_SEALED_OPAM_ENV_KEYS = {
    "OPAM_SWITCH_PREFIX",
    "OPAMSWITCH",
    "CAML_LD_LIBRARY_PATH",
    "OCAML_TOPLEVEL_PATH",
    "OCAMLTOP_INCLUDE_PATH",
}
_SEALED_OPAM_PATH_KEYS = _SEALED_OPAM_ENV_KEYS - {"OPAMSWITCH"}


def _load_sealed_opam_environment(path: Path, opam_switch: str) -> Dict[str, str]:
    try:
        raw = path.read_bytes()
        value = json.loads(raw)
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise SystemExit(f"cannot read sealed opam environment {path}: {exc}") from exc
    canonical = json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")
    if canonical != raw or not isinstance(value, dict) or set(value) != _SEALED_OPAM_ENV_KEYS:
        raise SystemExit("sealed opam environment is not the canonical approved object")
    if any(
        not isinstance(key, str)
        or not isinstance(item, str)
        or "\0" in key + item
        for key, item in value.items()
    ):
        raise SystemExit("sealed opam environment contains an invalid entry")
    prefix_raw = value["OPAM_SWITCH_PREFIX"]
    try:
        prefix = Path(prefix_raw).resolve(strict=True)
    except OSError as exc:
        raise SystemExit(f"sealed opam switch prefix is unavailable: {prefix_raw}") from exc
    if prefix_raw != str(prefix) or not prefix.is_dir():
        raise SystemExit("sealed opam switch prefix is not a normalized directory")
    if value["OPAMSWITCH"] != opam_switch:
        raise SystemExit("sealed opam environment selected a different switch")
    for key in sorted(_SEALED_OPAM_PATH_KEYS):
        paths = value[key].split(os.pathsep)
        if not paths or any(not item for item in paths):
            raise SystemExit(f"sealed opam variable {key} has an empty path")
        for item in paths:
            try:
                resolved = Path(item).resolve(strict=True)
            except OSError as exc:
                raise SystemExit(
                    f"sealed opam variable {key} names an unavailable path: {item}"
                ) from exc
            if not resolved.is_relative_to(prefix):
                raise SystemExit(
                    f"sealed opam variable {key} escapes the switch prefix"
                )
    return {key: value[key] for key in sorted(value)}


def build_extraction_env(
    charon_bin: Path,
    opam_switch: str,
    opam_env_json: Optional[Path] = None,
) -> Dict[str, str]:
    """A clean env with charon on PATH and the opam switch active.

    `opam env --switch=<s> --set-switch` is sourced so aeneas's path-pinned
    OCaml deps (charon-ml etc.) resolve. ``RUSTUP_TOOLCHAIN`` remains fixed to
    the date-pinned nightly used to build Charon.
    """
    # A closed child environment is both reproducible and safe to package as
    # evidence.  In particular, do not inherit provider tokens, SSH agent
    # sockets, proxy credentials, or unrelated build flags from the operator's
    # shell.  opam's exact switch variables are added below.
    env = _closed_host_tool_env()
    if opam_env_json is not None:
        env.update(_load_sealed_opam_environment(opam_env_json, opam_switch))
        return env
    charon_dir = str(charon_bin.parent)
    env["PATH"] = charon_dir + os.pathsep + env.get("PATH", "")
    try:
        opam_out = subprocess.run(
            ["opam", "env", f"--switch={opam_switch}", "--set-switch", "--sexp"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    except (subprocess.CalledProcessError, FileNotFoundError) as exc:
        raise SystemExit(f"opam env --switch={opam_switch} failed: {exc}") from exc
    # Parse the s-expression form: ((VAR "VALUE") ...). Robust to quoting.
    for var, val in re.findall(r'\("([^"]+)"\s+"((?:[^"\\]|\\.)*)"\)', opam_out):
        env[var] = val.encode().decode("unicode_escape")
    # Re-prepend charon so the opam PATH cannot shadow it.
    env["PATH"] = charon_dir + os.pathsep + env["PATH"]
    return env


@dataclass
class PipelineStepEvidence:
    tool: str
    argv: List[str]
    cwd: str
    effective_environment: Dict[str, str]
    exit_status: int
    stdout: bytes
    stderr: bytes


@dataclass
class PipelineRunEvidence:
    run_role: str
    steps: List[PipelineStepEvidence]
    generated_lean_sha256: str = ""
    llbc_sha256: str = ""
    tablet_root_sha256: str = ""
    extraction_toolchain: Dict[str, object] = field(default_factory=dict)
    not_recorded: List[Dict[str, str]] = field(default_factory=list)
    extraction_provenance: Dict[str, object] = field(default_factory=dict)
    cargo_metadata: Optional[dict] = None


EXTRACTOR_TYPED_RESULT_SCHEMA = "trellis-pv-extractor-typed-result/v1"


@dataclass
class PipelineFailureResult:
    stage: str
    detail: str
    run_role: str
    steps: List[PipelineStepEvidence]


class PipelineStageFailure(SystemExit):
    """A production-compatible ``SystemExit`` carrying exact partial evidence."""

    def __init__(self, result: PipelineFailureResult):
        super().__init__(result.detail)
        self.result = result


def _pipeline_failure(
    stage: str,
    detail: str,
    run_role: str,
    steps: List[PipelineStepEvidence],
) -> PipelineStageFailure:
    return PipelineStageFailure(
        PipelineFailureResult(
            stage=stage,
            detail=detail,
            run_role=run_role,
            steps=list(steps),
        )
    )


def _typed_step(step: PipelineStepEvidence) -> dict:
    return {
        "tool": step.tool,
        "argv": step.argv,
        "cwd": step.cwd,
        "environment": step.effective_environment,
        "exit_status": step.exit_status,
        "stdout_base64": __import__("base64").b64encode(step.stdout).decode("ascii"),
        "stdout_byte_length": len(step.stdout),
        "stdout_sha256": hashlib.sha256(step.stdout).hexdigest(),
        "stderr_base64": __import__("base64").b64encode(step.stderr).decode("ascii"),
        "stderr_byte_length": len(step.stderr),
        "stderr_sha256": hashlib.sha256(step.stderr).hexdigest(),
    }


def _write_typed_result(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(_canonical_json_bytes(value))


def _not_recorded(
    entries: List[Dict[str, str]], field_name: str, reason: str
) -> None:
    """Record one honest provenance gap, once, in deterministic order."""
    if not any(entry.get("field") == field_name for entry in entries):
        entries.append({"field": field_name, "reason": reason})


def _decode_probe_output(raw: bytes, *, field_name: str) -> str:
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise SystemExit(
            f"cannot record {field_name}: tool output is not valid UTF-8"
        ) from exc


def _extractor_git_revision(
    tool: str,
    executable: Path,
    not_recorded: List[Dict[str, str]],
) -> Optional[str]:
    """Return the full source revision containing an extractor executable.

    Production trust-v1 extraction separately requires a clean checkout and
    binds its origin.  Plain/synthetic extraction may use a standalone fixture
    executable; that is an honest negative here, not an empty revision.
    """
    git_env = _closed_host_tool_env()
    root_probe = subprocess.run(
        ["git", "-C", str(executable.parent), "rev-parse", "--show-toplevel"],
        env=git_env,
        check=False,
        capture_output=True,
    )
    if root_probe.returncode != 0:
        _not_recorded(
            not_recorded,
            f"{tool}_revision",
            "extractor executable is not in a readable Git checkout",
        )
        return None
    try:
        root = Path(root_probe.stdout.decode("utf-8").strip()).resolve(strict=True)
        executable.resolve(strict=True).relative_to(root)
    except (UnicodeDecodeError, OSError, ValueError):
        _not_recorded(
            not_recorded,
            f"{tool}_revision",
            "extractor executable is outside its resolved Git checkout",
        )
        return None
    clean = subprocess.run(
        ["git", "-C", str(root), "status", "--porcelain", "--untracked-files=all"],
        env=git_env,
        check=False,
        capture_output=True,
    )
    if clean.returncode != 0 or clean.stdout.strip():
        _not_recorded(
            not_recorded,
            f"{tool}_revision",
            "extractor source checkout is not clean enough to identify one revision",
        )
        return None
    completed = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "HEAD"],
        env=git_env,
        check=False,
        capture_output=True,
    )
    revision = completed.stdout.decode("ascii", errors="ignore").strip()
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        _not_recorded(
            not_recorded,
            f"{tool}_revision",
            "extractor checkout did not report a full 40-hex Git revision",
        )
        return None
    return revision


def _resolved_cargo_features(metadata: object, crate_src: Path) -> List[dict]:
    """Normalize Cargo's resolved per-package feature set without scratch paths."""
    if not isinstance(metadata, dict):
        raise ValueError("Cargo metadata root is not an object")
    packages = metadata.get("packages")
    resolve = metadata.get("resolve")
    if not isinstance(packages, list) or not isinstance(resolve, dict):
        raise ValueError("Cargo metadata lacks packages/resolve")
    nodes = resolve.get("nodes")
    if not isinstance(nodes, list):
        raise ValueError("Cargo metadata resolve lacks nodes")
    package_by_id = {
        package.get("id"): package
        for package in packages
        if isinstance(package, dict) and isinstance(package.get("id"), str)
    }
    records: List[dict] = []
    for node in nodes:
        if not isinstance(node, dict) or not isinstance(node.get("id"), str):
            raise ValueError("Cargo metadata resolve contains a malformed node")
        package = package_by_id.get(node["id"])
        if package is None:
            raise ValueError("Cargo metadata resolve names an unknown package")
        name = package.get("name")
        version = package.get("version")
        manifest_path = package.get("manifest_path")
        source = package.get("source")
        if not isinstance(name, str) or not name:
            raise ValueError("Cargo metadata package has no name")
        if not isinstance(version, str) or not version:
            raise ValueError("Cargo metadata package has no version")
        if not isinstance(manifest_path, str) or not manifest_path:
            raise ValueError("Cargo metadata package has no manifest_path")
        if source is not None and (not isinstance(source, str) or not source):
            raise ValueError("Cargo metadata package has a malformed source")
        if source is None:
            try:
                relative_manifest = Path(str(manifest_path)).resolve().relative_to(
                    crate_src.resolve()
                ).as_posix()
                source = f"workspace:{relative_manifest}"
            except (OSError, ValueError):
                # An external path dependency is an input Cargo resolved but the
                # present source copier cannot package.  Do not leak an unstable
                # absolute scratch path into otherwise deterministic provenance.
                source = "path-dependency"
        features = node.get("features")
        if not isinstance(features, list) or not all(
            isinstance(feature, str) for feature in features
        ):
            raise ValueError("Cargo metadata node has a malformed feature list")
        records.append(
            {
                "name": name,
                "version": version,
                "source": str(source),
                "features": sorted(features),
            }
        )
    records.sort(
        key=lambda record: (
            record["name"].encode("utf-8"),
            record["version"].encode("utf-8"),
            record["source"].encode("utf-8"),
        )
    )
    return records


def run_pipeline(
    crate_src: Path,
    out_dir: Path,
    *,
    charon_bin: Path,
    aeneas_bin: Path,
    env: Dict[str, str],
    cargo_target_dir: Path,
    run_role: str,
    charon_version: str,
    aeneas_version: str,
    lean_version: str,
    target_triple: Optional[str] = None,
    extraction_profile: str = "dev",
    strict_phase0: bool = False,
    extraction_config: Optional[dict] = None,
) -> Tuple[Path, Path, PipelineRunEvidence]:
    """charon -> LLBC -> aeneas with complete process evidence.

    Returns the generated Lean path and the exact closed-environment command,
    stdout/stderr, and exit-status record for both steps.

    Runs charon FROM a clean copy of the crate (caller-provided `crate_src`) so
    the source crate's own `target/` is never touched. A per-call
    `CARGO_TARGET_DIR` keeps each extraction's compile artifacts isolated (and
    "clean target" for the A/B determinism gate).

    The FILESPEC splitter currently consumes one generated Lean module.
    Supplied extraction arguments are preserved; incompatible module output
    fails before the bootstrap can accept it.
    """
    out_dir.mkdir(parents=True, exist_ok=True)
    llbc = out_dir / "m.llbc"
    source_tools = extraction_config.get("source_tools") if extraction_config is not None else None
    if source_tools is not None:
        if not isinstance(source_tools, dict):
            raise ValueError("extraction source_tools must be a driver-supplied tool configuration")
        from trellis.modeling.environment import source_environment
        run_env = source_environment(source_tools, out_dir / "execution", base_env=env)
    else:
        run_env = dict(env)
    run_env["CARGO_TARGET_DIR"] = str(cargo_target_dir)
    steps: List[PipelineStepEvidence] = []
    not_recorded: List[Dict[str, str]] = []

    # Record the exact compiler, including commit/date/host/release/LLVM, before
    # allowing Cargo/Charon to interpret the source.  Use rustup's selected real
    # executable rather than merely recording the mutable proxy path.
    rustc_bin = _toolchain_executable_for_cwd("rustc", run_env, crate_src)
    rustc_argv = [str(rustc_bin), "-vV"]
    rustc = subprocess.run(
        rustc_argv,
        cwd=str(crate_src),
        env=run_env,
        check=False,
        capture_output=True,
    )
    steps.append(
        PipelineStepEvidence(
            tool="rustc-version",
            argv=rustc_argv,
            cwd=str(crate_src),
            effective_environment=dict(sorted(run_env.items())),
            exit_status=rustc.returncode,
            stdout=rustc.stdout,
            stderr=rustc.stderr,
        )
    )
    if rustc.returncode != 0:
        raise _pipeline_failure(
            "rust_identity",
            "rustc -vV provenance probe failed with status "
            f"{rustc.returncode}: "
            + rustc.stderr.decode("utf-8", errors="replace"),
            run_role,
            steps,
        )
    rustc_vv = _decode_probe_output(rustc.stdout, field_name="rustc_vv")
    if not rustc_vv.strip():
        raise SystemExit("rustc -vV returned empty output")
    host_match = re.search(r"^host:\s*(\S+)\s*$", rustc_vv, re.MULTILINE)
    selected_target = (target_triple or "").strip()
    if not selected_target:
        if host_match is None:
            raise SystemExit("rustc -vV did not report a host target triple")
        selected_target = host_match.group(1)
    if not re.fullmatch(r"[A-Za-z0-9_.-]+", selected_target):
        raise SystemExit(f"invalid extraction target triple: {selected_target!r}")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+", extraction_profile):
        raise SystemExit(f"invalid Cargo extraction profile: {extraction_profile!r}")
    for field_name, value in (
        ("charon", charon_version),
        ("aeneas", aeneas_version),
        ("lean", lean_version),
    ):
        if not value.strip():
            raise SystemExit(f"empty extraction toolchain identity: {field_name}")
    # Pin Cargo's implicit target as well as passing --target explicitly below;
    # a crate-local Cargo config cannot silently select a different target.
    run_env["CARGO_BUILD_TARGET"] = selected_target

    extraction_toolchain: Dict[str, object] = {
        "charon": charon_version,
        "aeneas": aeneas_version,
        "lean": lean_version,
        "rustc_vv": rustc_vv,
        "target_triple": selected_target,
        "extraction_profile": extraction_profile,
    }
    for tool, executable in (("charon", charon_bin), ("aeneas", aeneas_bin)):
        revision = _extractor_git_revision(tool, executable, not_recorded)
        if revision is not None:
            extraction_toolchain[f"{tool}_revision"] = revision

    def execute(tool: str, argv: List[str]) -> subprocess.CompletedProcess:
        if source_tools is None:
            return run_compute(argv, cwd=str(crate_src), env=run_env,
                                  check=False, capture_output=True)
        from trellis.modeling.environment import run_source_command
        from trellis.pv_execution import execution_timeout
        stdout = out_dir / "execution" / f"{tool}.stdout"
        stderr = out_dir / "execution" / f"{tool}.stderr"
        result = run_source_command(argv, cwd=crate_src, env=run_env,
                                    stdout=stdout, stderr=stderr,
                                    timeout=execution_timeout(source_tools, tool))
        return subprocess.CompletedProcess(argv, result["exit_code"], stdout.read_bytes(), stderr.read_bytes())

    charon_argv = [
        str(charon_bin),
        "cargo",
        "--preset=aeneas",
        "--dest-file",
        str(llbc),
        "--",
        "--offline",
        "--profile",
        extraction_profile,
        "--target",
        selected_target,
    ]
    arguments = extraction_arguments({} if extraction_config is None else extraction_config)
    separator = charon_argv.index("--")
    charon_argv[separator:separator] = arguments["charon_args"]
    charon_argv.extend(arguments["cargo_args"])
    if (crate_src / "Cargo.lock").is_file():
        charon_argv.append("--locked")
    charon = execute("charon", charon_argv)
    steps.append(
        PipelineStepEvidence(
            tool="charon",
            argv=charon_argv,
            cwd=str(crate_src),
            effective_environment=dict(sorted(run_env.items())),
            exit_status=charon.returncode,
            stdout=charon.stdout,
            stderr=charon.stderr,
        )
    )
    if charon.returncode != 0:
        raise _pipeline_failure(
            "charon",
            "charon extraction failed with status "
            f"{charon.returncode}: {charon.stderr.decode('utf-8', errors='replace')}",
            run_role,
            steps,
        )
    if not llbc.is_file():
        raise _pipeline_failure(
            "charon", f"charon did not produce LLBC at {llbc}", run_role, steps
        )

    # Charon has now resolved exactly the Cargo graph it extracted.  Ask Cargo
    # for that locked graph and its effective root-crate cfg under the same
    # target/profile/environment.  These probes are evidence-only; when a
    # synthetic Cargo cannot answer, retain the successful extraction and name
    # the unavailable fields with reasons instead of inventing values.
    cargo_bin = _toolchain_executable_for_cwd("cargo", run_env, crate_src)
    metadata_argv = [
        str(cargo_bin),
        "metadata",
        "--format-version=1",
        "--locked",
        "--offline",
        "--filter-platform",
        selected_target,
    ]
    metadata = subprocess.run(
        metadata_argv,
        cwd=str(crate_src),
        env=run_env,
        check=False,
        capture_output=True,
    )
    steps.append(
        PipelineStepEvidence(
            tool="cargo-metadata",
            argv=metadata_argv,
            cwd=str(crate_src),
            effective_environment=dict(sorted(run_env.items())),
            exit_status=metadata.returncode,
            stdout=metadata.stdout,
            stderr=metadata.stderr,
        )
    )
    resolved_cargo_metadata: Optional[dict] = None
    metadata_issue = ""
    if metadata.returncode == 0:
        try:
            parsed_metadata = json.loads(
                _decode_probe_output(metadata.stdout, field_name="cargo_features")
            )
            extraction_toolchain["cargo_features"] = _resolved_cargo_features(
                parsed_metadata, crate_src
            )
            if isinstance(parsed_metadata, dict):
                resolved_cargo_metadata = parsed_metadata
        except (json.JSONDecodeError, ValueError) as exc:
            metadata_issue = f"Cargo metadata output could not be normalized: {exc}"
            _not_recorded(
                not_recorded,
                "cargo_features",
                f"Cargo metadata output could not be normalized: {exc}",
            )
    else:
        metadata_issue = "cargo metadata --locked --offline failed after extraction"
        _not_recorded(
            not_recorded,
            "cargo_features",
            "cargo metadata --locked --offline failed after extraction",
        )

    lockfile = crate_src / "Cargo.lock"
    if lockfile.is_file():
        extraction_toolchain["cargo_lock_sha256"] = hashlib.sha256(
            lockfile.read_bytes()
        ).hexdigest()
    else:
        _not_recorded(
            not_recorded,
            "cargo_lock_sha256",
            "Cargo produced no resolved Cargo.lock in the extraction copy",
        )

    cfg_argv = [
        str(cargo_bin),
        "rustc",
        "--lib",
        "--locked",
        "--offline",
        "--profile",
        extraction_profile,
        "--target",
        selected_target,
        "--",
        "--print",
        "cfg",
    ]
    cfg_probe = subprocess.run(
        cfg_argv,
        cwd=str(crate_src),
        env=run_env,
        check=False,
        capture_output=True,
    )
    steps.append(
        PipelineStepEvidence(
            tool="cargo-rustc-cfg",
            argv=cfg_argv,
            cwd=str(crate_src),
            effective_environment=dict(sorted(run_env.items())),
            exit_status=cfg_probe.returncode,
            stdout=cfg_probe.stdout,
            stderr=cfg_probe.stderr,
        )
    )
    cfg_values: List[str] = []
    if cfg_probe.returncode == 0:
        cfg_values = sorted(
            line
            for line in _decode_probe_output(
                cfg_probe.stdout, field_name="cfg"
            ).splitlines()
            if line
        )
    if cfg_values:
        extraction_toolchain["cfg"] = cfg_values
        extraction_toolchain["overflow_checks"] = "overflow_checks" in cfg_values
        panic_values = [
            match.group(1)
            for value in cfg_values
            if (match := re.fullmatch(r'panic="([^"]+)"', value))
        ]
        pointer_widths = [
            int(match.group(1))
            for value in cfg_values
            if (match := re.fullmatch(r'target_pointer_width="(\d+)"', value))
        ]
        if len(panic_values) == 1:
            extraction_toolchain["panic_strategy"] = panic_values[0]
        else:
            _not_recorded(
                not_recorded,
                "panic_strategy",
                "effective Cargo rustc cfg did not report exactly one panic value",
            )
        if len(pointer_widths) == 1:
            extraction_toolchain["target_pointer_width"] = pointer_widths[0]
        else:
            _not_recorded(
                not_recorded,
                "target_pointer_width",
                "effective Cargo rustc cfg did not report exactly one pointer width",
            )
    else:
        reason = "cargo rustc --print cfg failed for the extracted target/profile"
        for field_name in (
            "cfg",
            "overflow_checks",
            "panic_strategy",
            "target_pointer_width",
        ):
            _not_recorded(not_recorded, field_name, reason)
    if strict_phase0 and (metadata_issue or cfg_probe.returncode != 0 or not cfg_values):
        detail = metadata_issue or (
            "cargo rustc --print cfg failed for the extracted target/profile"
        )
        raise _pipeline_failure(
            "cargo_metadata_cfg", detail, run_role, steps
        )
    aeneas_argv = [
        str(aeneas_bin),
        "-backend",
        "lean",
        "-dest",
        str(out_dir),
        *arguments["aeneas_args"],
        str(llbc),
    ]
    aeneas = execute("aeneas", aeneas_argv)
    steps.append(
        PipelineStepEvidence(
            tool="aeneas",
            argv=aeneas_argv,
            cwd=str(crate_src),
            effective_environment=dict(sorted(run_env.items())),
            exit_status=aeneas.returncode,
            stdout=aeneas.stdout,
            stderr=aeneas.stderr,
        )
    )
    if aeneas.returncode != 0:
        raise _pipeline_failure(
            "aeneas",
            "aeneas extraction failed with status "
            f"{aeneas.returncode}: {aeneas.stderr.decode('utf-8', errors='replace')}",
            run_role,
            steps,
        )
    leans = sorted(p for p in out_dir.glob("*.lean"))
    if len(leans) != 1:
        raise _pipeline_failure(
            "aeneas",
            f"expected exactly one generated Lean under {out_dir}, found {leans}",
            run_role,
            steps,
        )
    not_recorded.sort(key=lambda entry: entry["field"].encode("utf-8"))
    return leans[0], llbc, PipelineRunEvidence(
        run_role=run_role,
        steps=steps,
        extraction_toolchain=extraction_toolchain,
        not_recorded=not_recorded,
        cargo_metadata=resolved_cargo_metadata,
    )


# ─────────────────────────── FILESPEC split ────────────────────────────────


@dataclass
class ModelNode:
    name: str
    text: str  # the full Tablet/<name>.lean file content (with markers)
    region: str  # prescribed_region(include_body=true) — the config `lean`
    deps: List[str] = field(default_factory=list)  # sibling Tablet nodes imported


@dataclass
class BoundaryAxiom:
    """One disclosed Tier-B boundary axiom routed to the Preamble.

    The deterministic TCB-disclosure unit emitted by the extractor. `name` keeps
    the existing manifest naming form; setup derives closure-gate names from it.
    """

    name: str
    classification: str
    stubs: str  # the axiom's Lean type signature (the thing it stubs)
    item_path: str  # the `[…]` source item from the leading docstring ("" if none)


@dataclass
class SplitResult:
    namespace: str
    preamble_text: str
    nodes: List[ModelNode] = field(default_factory=list)
    # Tier-B boundary axioms routed to the Preamble, in SOURCE order — the
    # deterministic TCB-disclosure set (feeds `--emit-tcb-manifest`).
    boundary_axioms: List[BoundaryAxiom] = field(default_factory=list)
    # Exact Aeneas external declarations replaced by reviewed executable
    # definitions. Name or signature drift remains a disclosed axiom.
    boundary_definitions: List[dict] = field(default_factory=list)
    # Original external declarations, before any seed-authorized replacement.
    # Native model authoring uses these identities without inheriting seed trust.
    raw_boundary_declarations: List[dict] = field(default_factory=list)
    validity_definitions: List[dict] = field(default_factory=list)
    type_nodes: List[str] = field(default_factory=list)


def _seed_tcb_axioms() -> List[BoundaryAxiom]:
    return []


def _item_path(doc_line: str) -> Optional[str]:
    """The `[crate::item]` path from a docstring opening line.

    aeneas writes the item path inside the FIRST `/--` line, but sometimes
    behind a prefix (`Trait declaration: [..]`, `Trait implementation: [..]`).
    The path itself can be noisy (`{impl avl::Ord for i32}::cmp`,
    `{avl::Node<T>}::rotate_left`) and can NEST `[…]` brackets inside (a slice
    method's path is `[core::slice::{[T]}::first]` — the inner `[T]` is part of
    the path). We return the raw bracketed contents, matched with BALANCED `[…]`
    so the inner `[T]` does not truncate the path (which would collapse distinct
    items — `…::first` vs `…::split_first` — to the same prefix and wrongly merge
    their blocks); the node-name derivation normalizes it.
    """
    start = doc_line.find("[")
    if start == -1:
        return None
    depth = 0
    for idx in range(start, len(doc_line)):
        ch = doc_line[idx]
        if ch == "[":
            depth += 1
        elif ch == "]":
            depth -= 1
            if depth == 0:
                # The bracketed contents, excluding the outer `[` and `]`.
                return doc_line[start + 1: idx]
    # Unbalanced (no closing `]` on this line) — fall back to the non-balanced
    # scan so behavior degrades to "everything after the first `[`".
    return doc_line[start + 1:]


def _node_name_from_item(item: str) -> str:
    """Derive a Lean-identifier node name (= file stem = principal decl name).

    Examples (aeneas item path -> node name = generated decl name):
      avl::add1                              -> add1
      avl::gcd                               -> gcd
      avl::Ordering                          -> Ordering
      avl::{impl avl::Ord for i32}::cmp      -> I32.Insts.AvlOrd.cmp  (best-effort)
      avl::{avl::Node<T>}::rotate_left       -> Node.rotate_left
      avl::{avl::Tree<T>}::insert_in_opt_node-> Tree.insert_in_opt_node

    The generated `def` name is authoritative; this is only a fallback when no
    `def`/`inductive`/... head is found in the block. `split_generated_lean`
    prefers the actual declared name (`_principal_decl_name`).
    """
    # Strip the leading crate:: segment.
    s = re.sub(r"^[A-Za-z_][A-Za-z0-9_]*::", "", item)
    # `{impl ... for Ty}::method` -> Ty + method (heuristic). `{Ty<..>}::m` -> Ty + m.
    m = re.match(r"^\{[^}]*\bfor\s+([A-Za-z0-9_:<> ]+)\}::(.+)$", s)
    if m:
        ty = m.group(1).strip().split("<")[0].split("::")[-1]
        return f"{ty}.{m.group(2)}"
    m = re.match(r"^\{([A-Za-z0-9_:<> ]+)\}::(.+)$", s)
    if m:
        ty = m.group(1).strip().split("<")[0].split("::")[-1]
        return f"{ty}.{m.group(2)}"
    return s


def _principal_decl_name(block: List[str]) -> Optional[str]:
    """The first declared name in a block (the principal declaration)."""
    return next(iter(_declared_names(block)), None)


def _strip_trailing_blank(block: List[str]) -> List[str]:
    out = list(block)
    while out and out[-1].strip() == "":
        out.pop()
    return out


def _find_body_split(block: List[str]) -> Tuple[int, Optional[Tuple[str, str]]]:
    """Locate the first line carrying the principal `:=` body delimiter.

    Returns `(idx, break)`. If `break is None`, the whole line `block[idx]`
    already ends in `:=`/`by`/`where` and the split is right after it. Otherwise
    `break = (head, tail)` splits `block[idx]` right after its `:=` so the head
    (above `-- BODY`) ends in `:=` and `tail` (e.g. ` do`) moves below.
    """
    for idx, ln in enumerate(block):
        st = ln.rstrip()
        if (
            st.endswith(":=")
            or st == "by"
            or st.endswith(" by")
            or st == "where"
            or st.endswith(" where")
        ):
            return idx, None
        m = re.search(r":=", ln)
        if m:
            return idx, (ln[: m.end()], ln[m.end():])
    raise SystemExit(
        "FILESPEC split: no `:=`/`by`/`where` body delimiter found in declaration "
        f"group starting {block[0]!r}; cannot place `-- BODY`"
    )


def split_generated_lean(
    generated: str, model_boundary_contract: Optional[dict] = None, *,
    expose_types: bool = False,
) -> SplitResult:
    """Deterministically split one aeneas-generated Lean file into nodes.

    Pure text, no Lean/lake access. Deterministic shaping over deterministic
    input (the generated Lean is byte-stable).
    """
    boundary_contract = model_boundary_contract or {}
    lines = generated.split("\n")

    ns_match = next(
        (re.match(r"^namespace\s+(\S+)\s*$", ln) for ln in lines if re.match(r"^namespace\s+(\S+)\s*$", ln)),
        None,
    )
    if ns_match is None:
        raise SystemExit("FILESPEC split: no `namespace <crate>` line in generated Lean")
    namespace = ns_match.group(1)

    first_doc = next((i for i, ln in enumerate(lines) if ln.startswith("/--")), None)
    if first_doc is None:
        raise SystemExit("FILESPEC split: no top-level docstring (`/--`) in generated Lean")
    end_idx = next(
        (i for i, ln in enumerate(lines) if re.match(rf"^end\s+{re.escape(namespace)}\s*$", ln)),
        None,
    )
    if end_idx is None:
        raise SystemExit(f"FILESPEC split: no closing `end {namespace}` in generated Lean")

    header_lines = lines[:first_doc]
    body_lines = lines[first_doc:end_idx]

    # `noncomputable section` (aeneas's blanket, emitted unless `-all-computable`)
    # opens in the header. A Lean `section` is FILE-LOCAL (like `open`), so it does
    # NOT cross `import Tablet.Preamble`; to keep each split node file as
    # noncomputable as the single generated file, the flag is re-emitted in every
    # node's free header below (only when the source had it — montgomery does not,
    # so its node files stay byte-identical).
    noncomputable = any(re.match(r"^\s*noncomputable\s+section\b", ln) for ln in header_lines)

    # Preamble = header minus the `namespace` line (node files are standalone).
    preamble_body = [ln for ln in header_lines if not re.match(r"^namespace\s", ln)]

    blocks = _segment_into_blocks(body_lines)

    # Legacy layout routes boundary-axiom and type-definition blocks to Preamble.
    # Native modeling exposes types as pinned nodes so authored bindings can
    # import them. The remaining commentary describes the legacy layout.
    # ── Route boundary-axiom and type-definition blocks to the Preamble (the
    # import ROOT — `lake build Tablet.Preamble` is the build foundation, and the
    # Preamble cannot import a node). Two shape classes go here, in SOURCE order:
    #   * axiom-only blocks — the disclosed Tier-B boundary axioms (Aeneas-stdlib
    #     prims like `Slice.first`/`Option…branch`, plus auto-derived abstract
    #     methods like a struct's `PartialEq::ne`). An `axiom` has no
    #     `:=`/`by`/`where` body so it cannot be a `-- BODY`-bearing node. (The
    #     auto-derived `…ne` axioms are KEPT, not dropped: at least one is a live
    #     call site and the rest are referenced by their `PartialEq` instance
    #     record, so dropping is neither sound-trivial nor general — full
    #     disclosure in the Preamble is the faithful choice.)
    #   * type-definition blocks (`structure`/`inductive` — the Rust struct/enum) —
    #     leaf data declarations referencing only Aeneas/`Std`. They MUST sit in the
    #     Preamble so a boundary `axiom` over the type (a derived `…ne`) resolves,
    #     and so every ordinary tablet node stays a genuine body-bearing `def`.
    # Both are re-emitted verbatim, in source order, INSIDE the re-opened
    # `namespace <crate>` (the names are namespace-qualified) and inside
    # `noncomputable section` if present. Aeneas's trait-instance `def`s carry their
    # OWN docstrings → separate blocks → they remain function nodes. NOT crate-
    # specific: any axiom-only / type-definition block routes here.
    preamble_decl_blocks: List[List[str]] = []
    node_blocks: List[List[str]] = []
    boundary_axioms: List[BoundaryAxiom] = []
    boundary_definitions: List[dict] = []
    raw_boundary_declarations: List[dict] = []
    for block in blocks:
        stripped = _strip_trailing_blank(block)
        if expose_types and stripped and _is_type_decl_block(stripped):
            node_blocks.append(stripped)
            continue
        if stripped and (_is_axiom_only_block(stripped) or _is_type_decl_block(stripped)):
            if _is_axiom_only_block(stripped):
                identity = boundary_declaration_identity(stripped)
                if identity is not None:
                    raw_boundary_declarations.append(identity)
                replacement = _reviewed_boundary_definition(
                    stripped, boundary_contract
                )
                if replacement is not None:
                    definition_block, definition_record = replacement
                    preamble_decl_blocks.append(definition_block)
                    boundary_definitions.append(definition_record)
                else:
                    preamble_decl_blocks.append(stripped)
                    boundary_axioms.extend(_boundary_axioms_from_block(stripped, namespace))
            else:
                preamble_decl_blocks.append(stripped)
        else:
            node_blocks.append(block)
    blocks = node_blocks

    preamble_parts = ["\n".join(preamble_body).rstrip("\n")]
    if preamble_decl_blocks:
        ax_lines: List[str] = [f"namespace {namespace}", ""]
        for ab in preamble_decl_blocks:
            ax_lines += ab
            ax_lines.append("")
        preamble_parts.append("\n".join(ax_lines).rstrip("\n"))
    preamble_text = "\n\n".join(p for p in preamble_parts if p).rstrip("\n") + "\n"
    preamble_text = prepare_generated_lean(preamble_text)

    # ── First pass: shape every block into (node_name, head_part, body_part) and
    # collect the names it DECLARES. Deterministic over the byte-stable input. ──
    @dataclass
    class _Shaped:
        name: str  # SANITIZED node id = file stem = marker = spec.name
        head_part: List[str]
        body_part: List[str]
        declared: List[str]  # every top-level declared name (ORIGINAL, may be dotted)
        is_type: bool

    shaped: List[_Shaped] = []
    seen_names: set[str] = {PREAMBLE_NODE, ASSUMPTIONS_NODE}
    for block in blocks:
        block = _strip_trailing_blank(block)
        if not block:
            continue
        if not _declared_names(block):
            raise ValueError(
                "FILESPEC split: unrecognized generated declaration; "
                f"cannot derive its identity and dependencies: {block[:4]!r}"
            )
        # The principal name carries aeneas's dotted qualification (`Node.rotate_left`,
        # `Tree.find`, `I32.Insts.AvlOrd.cmp`). A dot in a node id would make the
        # FILESPEC flat file `Tablet/<id>.lean` unreachable: Lean/lake resolve
        # `import Tablet.Node.rotate_left` to the NESTED path `Tablet/Node/rotate_left.lean`
        # (verified: `lake build Tablet` fails on the flat file), while the kernel's
        # olean convention is FLAT (`Tablet/<node>.olean`). Sanitize the dot to `_`
        # for the node id / file stem / marker / spec.name so the flat file IS the
        # module. The DECLARATION TEXT (the pinned region) is UNTOUCHED — it stays
        # `def Node.rotate_left …` under `namespace <crate>`, so the byte-pin pins the
        # real declaration and name-parity holds (node id == spec.name == file stem
        # == marker, all the sanitized form; the kernel never parses the Lean decl
        # name). Cross-file references still resolve: the sanitized module
        # `Tablet.Node_rotate_left` carries the decl `<crate>.Node.rotate_left`, which
        # a dependent reaches via `import Tablet.Node_rotate_left` + re-opened namespace.
        node_name = _sanitize_node_name(_block_node_name(block))
        # Guard against (improbable) duplicate stems by suffixing — keeps file
        # stems unique without colliding the loop-trio merge (which already
        # collapsed into one block above).
        base_name = node_name
        suffix = 0
        while node_name in seen_names:
            suffix += 1
            node_name = f"{base_name}__{suffix}"
        seen_names.add(node_name)
        idx, brk = _find_body_split(block)
        if brk is None:
            head_part, body_part = block[: idx + 1], block[idx + 1 :]
        else:
            head, tail = brk
            head_part = block[:idx] + [head]
            body_part = ([tail.lstrip()] if tail.strip() else []) + block[idx + 1 :]
        shaped.append(
            _Shaped(node_name, head_part, body_part, _declared_names(block), _is_type_decl_block(block))
        )

    # ── Build the tablet DAG: map EVERY declared name (principal AND co-generated
    # member — projectors, simp lemmas, the inner defs of a `mutual`/loop-trio
    # block) to its OWNING node. Keys are the ORIGINAL (possibly dotted) declared
    # names — that is what the decl text references (`Tree.insert_in_opt_node`,
    # declared inside the `Node.insert_in_left` mutual node); values are the
    # SANITIZED owner node id used to form `import Tablet.<owner>`. ──
    decl_owner: Dict[str, str] = {}
    for sh in shaped:
        for dname in sh.declared:
            # First writer wins (a name is declared once in the generated Lean);
            # deterministic over the byte-stable input.
            decl_owner.setdefault(dname, sh.name)

    # ── Second pass: per node, scan its decl text for references to OTHER nodes'
    # declared names → import exactly those `Tablet.<Sibling>` files. The header
    # (imports + `open` + `namespace <crate>`) sits ABOVE the `-- [TABLET NODE]`
    # marker, OUTSIDE the byte-pinned region. `namespace <crate>` carries NO
    # closing `end`: a namespace auto-closes at end of file, so the pinned region
    # (decl signature-through-body) stays the bare declaration — the byte-pin's
    # `prescribed_region(include_body=true)`. The re-opened `namespace <crate>`
    # is what lets a cross-file reference to a sibling (declared as
    # `<crate>.<Name>`) resolve after `import Tablet.<Sibling>`. ──
    result = SplitResult(
        namespace=namespace,
        preamble_text=preamble_text,
        boundary_axioms=boundary_axioms,
        boundary_definitions=boundary_definitions,
        raw_boundary_declarations=raw_boundary_declarations,
        validity_definitions=[
            dict(item)
            for item in boundary_contract.get("validity_definitions", [])
        ],
        type_nodes=[sh.name for sh in shaped if sh.is_type],
    )
    if expose_types:
        # Types remain byte-pinned extracted nodes. Preamble imports them so
        # external operations can mention them; authored bindings may import
        # the same nodes without acquiring a reverse edge to Preamble.
        result.preamble_text = "".join(f"import Tablet.{name}\n" for name in result.type_nodes) + preamble_text
    for sh in shaped:
        deps = _node_deps(sh.head_part + sh.body_part, sh.name, decl_owner)
        header: List[str] = (
            [ln for ln in header_lines if not re.match(r"^namespace\s", ln)
             and not re.match(r"^\s*noncomputable\s+section\s*$", ln)]
            if expose_types and sh.is_type else [f"import Tablet.{PREAMBLE_NODE}"]
        )
        dependency_imports = [f"import Tablet.{dep}" for dep in deps]
        header = dependency_imports + header if expose_types and sh.is_type else header + dependency_imports
        header.append("open Aeneas Aeneas.Std Result ControlFlow Error")
        # File-local elaboration settings stay outside pinned declarations.
        header.extend(f"set_option {name} {str(value).lower()}" for name, value in
                      generated_elaboration_options("\n".join(sh.body_part)).items())
        if noncomputable:
            # Re-open aeneas's blanket section (file-local; does not cross the
            # `import Tablet.Preamble`) so this node's decl is noncomputable like
            # the source. Free region (above the marker); a `section` is not part
            # of the kernel `namespace_context`, so it stays unpinned.
            header.append("noncomputable section")
        header.append(f"namespace {namespace}")
        file_lines = [
            *header,
            "",
            f"-- [TABLET NODE: {sh.name}]",
            *sh.head_part,
            "-- BODY",
            *sh.body_part,
        ]
        text = "\n".join(file_lines).rstrip("\n") + "\n"
        # The config `lean` is the byte-pinned `prescribed_region(include_body=true)`:
        # the declaration from (after) the `-- [TABLET NODE]` marker through end of
        # file with the `-- BODY` marker line excised, surrounding blank lines
        # trimmed. Because the header (imports/open/namespace) lives ABOVE the
        # marker and the namespace has no trailing `end`, that region is exactly
        # `head_part ++ body_part` — constructed here directly (NOT by re-parsing
        # markers). Gate (c) re-derives it from the on-disk file via the REAL
        # kernel `print_prescribed_region` and asserts byte-equality.
        region = _trim_surrounding_blank("\n".join(sh.head_part + sh.body_part))
        result.nodes.append(
            ModelNode(name=sh.name, text=text, region=region, deps=deps)
        )
    return result


def _segment_into_blocks(body_lines: List[str]) -> List[List[str]]:
    """Split the namespace body into per-node blocks.

    Rules (deterministic over the byte-stable generated Lean):
      * A block is led by a docstring (`/--`). A docstring-less decl (an aeneas
        auto-generated projector/simp-lemma, e.g. `def Node.value`, lifted out
        of an `inductive`) attaches to the CURRENT block — it is a co-generated
        member of that item.
      * `mutual ... end` is INDIVISIBLE: the whole block (with all its inner
        mutually-recursive `def`s + `partial_fixpoint` terminators) is ONE
        block, regardless of internal docstrings. Mutual recursion cannot be
        split into one-def-per-node files.
      * Consecutive docstring blocks sharing the same `[crate::item]` path merge
        into ONE block (the loop trio `f_loop.body`/`f_loop`/`f` — sign-off 1).
    """
    # First pass: tokenize into (kind, lines, item_path) segments at the top
    # level, treating a mutual block as one opaque segment.
    segments: List[Tuple[str, List[str], Optional[str]]] = []
    i = 0
    n = len(body_lines)
    while i < n:
        ln = body_lines[i]
        stripped = ln.strip()
        if stripped == "" :
            # Blank line between segments: skip (trailing blanks are restored by
            # the join; interior blanks within a segment are captured below).
            i += 1
            continue
        if re.match(r"^mutual\b", stripped):
            # Consume through the matching top-level `end`.
            depth = 1
            buf = [ln]
            i += 1
            item = None
            while i < n and depth > 0:
                cur = body_lines[i]
                cs = cur.strip()
                if item is None and cs.startswith("/--"):
                    item = _item_path(cur)
                if re.match(r"^mutual\b", cs):
                    depth += 1
                elif re.match(r"^end\b", cs):
                    depth -= 1
                buf.append(cur)
                i += 1
            segments.append(("mutual", buf, item))
            continue
        if stripped.startswith("/--"):
            # Docstring-led block: consume the docstring + following lines until
            # the next top-level docstring / mutual / EOF.
            item = _item_path(ln)
            buf = [ln]
            i += 1
            while i < n:
                cur = body_lines[i]
                cs = cur.strip()
                if cs.startswith("/--") or re.match(r"^mutual\b", cs):
                    break
                buf.append(cur)
                i += 1
            segments.append(("doc", buf, item))
            continue
        # A docstring-less decl at the top level (no current block to attach to,
        # or leading material). Consume until next docstring/mutual.
        buf = [ln]
        i += 1
        while i < n:
            cur = body_lines[i]
            cs = cur.strip()
            if cs.startswith("/--") or re.match(r"^mutual\b", cs):
                break
            buf.append(cur)
            i += 1
        segments.append(("bare", buf, None))

    # Second pass: attach `bare` segments to the preceding block; merge adjacent
    # `doc` segments with the same item path.
    blocks: List[List[str]] = []
    block_items: List[Optional[str]] = []
    for kind, buf, item in segments:
        if kind == "bare" and blocks:
            blocks[-1].append("")  # preserve the inter-decl blank line
            blocks[-1] += buf
            continue
        if (
            kind == "doc"
            and blocks
            and block_items
            and item is not None
            and block_items[-1] == item
        ):
            blocks[-1].append("")
            blocks[-1] += buf
            continue
        blocks.append(list(buf))
        block_items.append(item)
    return blocks


# A loop-auxiliary decl name. aeneas co-generates a function's loops as
# `f_loop<N>` (the loop driver) and `f_loop<N>.body` (its body), NUMBERED per
# loop, and NESTS them for inner loops (`f_loop<N>_loop<M>`, `…_loop<N>.body`).
# Older single-loop output used the bare `f_loop`/`f_loop.body` (no number). The
# entry point `f` is the declared name carrying NONE of these `_loop[<N>]`
# segments — `_loop` followed by optional digits, anywhere up to an optional
# trailing `.body`.
_LOOP_AUX_RE = re.compile(r"_loop\d*(?=$|_loop|\.body)")


def _is_loop_aux_name(name: str) -> bool:
    """True iff `name` is an aeneas loop driver/body (`f_loop`, `f_loop3`,
    `f_loop0.body`, `f_loop1_loop0`, `f_loop2_loop0.body`, …) — NOT the
    user-facing entry point `f`."""
    return _LOOP_AUX_RE.search(name) is not None


def _block_node_name(block: List[str]) -> str:
    """Node name for a block.

    Preference order:
      1. A loop trio (`@[rust_loop]`/`@[rust_loop_body]` present) is named after
         the user-facing entry point — the declared `def f` whose name carries
         none of aeneas's `_loop[<N>]`/`.body` loop-auxiliary suffixes (the trio,
         and all numbered/nested loops sharing the function's item path, are ONE
         node `f`).
      2. Otherwise the principal (first) declared name.
      3. Otherwise the item-path-derived name.
    """
    decl_names = _declared_names(block)
    is_loop_trio = any("rust_loop" in ln for ln in block)
    if is_loop_trio:
        entry = [n for n in decl_names if not _is_loop_aux_name(n)]
        if entry:
            return entry[0]
    if decl_names:
        return decl_names[0]
    for ln in block:
        if ln.strip().startswith("/--"):
            item = _item_path(ln)
            if item:
                return _node_name_from_item(item)
    return "Node"


def _sanitize_node_name(name: str) -> str:
    """A FILESPEC-legal flat node id (= file stem = module tail) from a decl name.

    aeneas qualifies many names with dots (`Node.rotate_left`, `Tree.find`,
    `I32.Insts.AvlOrd.cmp`). A dot would make `Tablet/<id>.lean` a NESTED module
    (`import Tablet.Node.rotate_left` → `Tablet/Node/rotate_left.lean`), which the
    flat FILESPEC layout + the kernel's flat `Tablet/<node>.olean` convention do
    not provide. Map each `.` to `_` so the flat file IS the module. Deterministic
    and stable over the byte-stable generated Lean. The declaration text is never
    touched — only the node id / file stem / marker / import target.
    """
    from trellis.checker.protocol import NODE_NAME_MAX_LEN, NODE_NAME_REGEX

    stem = name.replace(".", "_")
    if not NODE_NAME_REGEX.fullmatch(stem) or len(stem) > NODE_NAME_MAX_LEN - 12:
        # Lean permits identifiers beyond the flat ASCII module-name contract.
        # Preserve ordinary stems, and disambiguate encoded names deterministically.
        readable = re.sub(r"[^A-Za-z0-9_]", "_", stem)
        if not readable or not readable[0].isascii() or not readable[0].isalpha():
            readable = "Node_" + readable
        stem = readable[:NODE_NAME_MAX_LEN - 30] + "__" + hashlib.sha256(name.encode()).hexdigest()[:16]
    if not NODE_NAME_REGEX.fullmatch(stem) or len(stem) > NODE_NAME_MAX_LEN:
        raise ValueError(f"Invalid extracted node name: {stem!r}")
    return stem


def _declared_names(block: List[str]) -> List[str]:
    """Names owned by a body-bearing node, including co-generated declarations."""
    return [head.name for head in declaration_heads("\n".join(block)) if head.kind != "axiom"]


def _axiom_names(block: List[str]) -> List[str]:
    return [head.name for head in declaration_heads("\n".join(block)) if head.kind == "axiom"]


def _block_item_path(block: List[str]) -> str:
    """The `[crate::item]` path from the block's leading `/--` docstring ("" if none).

    aeneas precedes every emitted decl with a `/-- [<item>]: … -/` docstring; the
    bracketed item path is the deterministic provenance signal used to classify a
    boundary axiom (an Aeneas-stdlib prim has a `[core::…]`/`[alloc::…]`/… path;
    an auto-derived method has a `[<crate>::{impl <Trait> for …}::<m>]` path).
    """
    for ln in block:
        if ln.lstrip().startswith("/--"):
            path = _item_path(ln)
            return path or ""
    return ""


def _axiom_type_signature(block: List[str], axiom_name: str) -> str:
    """The exact Lean signature an `axiom` asserts, excluding its name.

    This is the thing the boundary axiom STUBS. For
    `axiom core.slice.Slice.first {T : Type} : Slice T → Result (Option T)` it is
    `{T : Type} : Slice T → Result (Option T)`. Captured from the code text
    only (comments stripped), reflow-tolerant: the axiom name may sit on the
    `axiom` line or reflow onto the next, and the signature may span lines.
    """
    code = _strip_lean_comments(block)
    # Collapse the axiom-only block's code to a single logical line. Binder groups
    # `{…}`/`(…)`/`[…]` before the result-type separator remain part of the
    # disclosed signature.
    flat = " ".join(part.strip() for part in code.splitlines() if part.strip())
    flat = re.sub(r"\s+", " ", flat).strip()
    # Anchor at the axiom name so a leading `@[…]`/`axiom` token is dropped.
    anchor = flat.find(axiom_name)
    if anchor != -1:
        flat = flat[anchor + len(axiom_name):].strip()
    # A binder-free declaration begins `: <type>`; drop only that separator.
    # Binder heads remain verbatim because they are part of the signature.
    if flat.startswith(":"):
        return flat[1:].strip()
    return flat.strip()


def boundary_declaration_identity(block: List[str]) -> Optional[dict]:
    """Identify one raw external declaration, retaining its complete telescope.

    The source digest uses the same declaration-block bytes as seed provenance:
    the extracted block, without trailing blank lines, followed by one newline.
    This describes an extraction boundary; it does not approve a replacement.
    """
    names = _axiom_names(block)
    if len(names) != 1 or not _is_axiom_only_block(block):
        return None
    name = names[0]
    code = " ".join(_strip_lean_comments(block).split())
    match = re.search(r"\baxiom\s+" + re.escape(name) + r"(?=\s|[:({])", code)
    if match is None:
        return None
    signature = code[match.end():].strip()
    axiom_line = next(i for i, line in enumerate(block)
                      if re.match(r"^\s*(?:@\[[^\]]*\]\s*)*axiom\b", line))
    source = "\n".join(block).rstrip("\n") + "\n"
    return {
        "name": name,
        "extracted_signature": signature,
        "type_signature": _axiom_type_signature(block, name),
        "rust_symbol": _block_item_path(block),
        "raw_declaration_sha256": hashlib.sha256(source.encode("utf-8")).hexdigest(),
        "source_utf8": source,
        "prefix_utf8": "\n".join(block[:axiom_line]) + ("\n" if axiom_line else ""),
    }


def match_boundary_declaration(
    identity: dict, *, name: str, rust_symbol: str,
    extracted_signature: Optional[str] = None,
    raw_declaration_sha256: Optional[str] = None,
    type_signature: Optional[str] = None,
) -> bool:
    """Shared exact matching for seed and ordinary authored boundary bindings."""
    return (identity["name"] == name and identity["rust_symbol"] == rust_symbol
            and (extracted_signature is None or identity["extracted_signature"] == extracted_signature)
            and (raw_declaration_sha256 is None or identity["raw_declaration_sha256"] == raw_declaration_sha256)
            and (type_signature is None or identity["type_signature"] == type_signature))


def _reviewed_boundary_definition(
    block: List[str],
    boundary_contract: dict,
) -> Optional[Tuple[List[str], dict]]:
    """Replace one exact Aeneas external declaration with its v1 definition.

    Selection is deliberately name-and-full-signature based. A new Aeneas
    spelling, type, binder shape, or multiple-axiom block remains an ordinary
    disclosed boundary axiom until explicitly reviewed.
    """
    identity = boundary_declaration_identity(block)
    if identity is None:
        return None
    name = identity["name"]
    reviewed = {
        item["name"]: item
        for item in boundary_contract.get("boundary_definitions", [])
    }.get(name)
    if reviewed is None:
        return None
    expected = reviewed["expected_extracted_lean_type_utf8"]
    definition_source_utf8 = reviewed["reviewed_definition_source_utf8"]
    definition = definition_source_utf8.rstrip("\n").split("\n")
    observed = identity["type_signature"]
    rust_symbol = identity["rust_symbol"]
    if not match_boundary_declaration(identity, name=name,
                                      rust_symbol=reviewed["rust_symbol_utf8"],
                                      type_signature=expected):
        return None

    axiom_line = next(
        index
        for index, line in enumerate(block)
        if re.match(r"^\s*(?:@\[[^\]]*\]\s*)*axiom\b", line)
    )
    replacement = [*block[:axiom_line], *definition]
    extracted_declaration_source_utf8 = identity["source_utf8"]
    definition_sha256 = hashlib.sha256(
        definition_source_utf8.encode("utf-8")
    ).hexdigest()
    extracted_declaration_sha256 = identity["raw_declaration_sha256"]
    monomorphizations = reviewed["approved_rust_monomorphizations"]
    review_scope = {
        "schema": "trellis-aeneas-boundary-review-scope/v1",
        "aeneas_symbol_utf8": name,
        "aeneas_type_signature_utf8": observed,
        "reviewed_definition_lean_type_utf8": reviewed[
            "reviewed_definition_lean_type_utf8"
        ],
        "rust_symbol_utf8": rust_symbol,
        "extracted_declaration_sha256": extracted_declaration_sha256,
        "generated_definition_sha256": definition_sha256,
        "rust_monomorphization_scope": monomorphizations,
        "evidence_class": (
            "exact-extracted-symbol-and-type-with-seed-authored-"
            "rust-instantiation-scope"
        ),
    }
    return replacement, {
        "name": name,
        "classification": "reviewed-executable-boundary-definition",
        "replaces_stubs": observed,
        "reviewed_definition_lean_type_utf8": reviewed[
            "reviewed_definition_lean_type_utf8"
        ],
        "rust_symbol_utf8": rust_symbol,
        "definition_source_utf8": definition_source_utf8,
        "definition_sha256": definition_sha256,
        "extracted_declaration_source_utf8": (
            extracted_declaration_source_utf8
        ),
        "extracted_declaration_sha256": extracted_declaration_sha256,
        "model_semantics": "trellis-aeneas-boundary-definitions/v1",
        "approved_rust_monomorphizations": monomorphizations,
        "review_scope": review_scope,
        "review_scope_sha256": hashlib.sha256(
            _canonical_json_bytes(review_scope)
        ).hexdigest(),
    }


def _classify_boundary_axiom(item_path: str, axiom_name: str, namespace: str) -> str:
    """Partition a boundary axiom into the two disclosed Tier-B kinds.

    Deterministic. The decisive signal is the OWNING namespace of the item the
    docstring attributes the axiom to:

      * "auto-derived-method" — an aeneas-derived trait method left abstract on a
        CRATE-OWNED type: the docstring item path is a trait-impl path
        `{impl <Trait> for <crate>::<Ty>}::<m>` whose impl-target type is in the
        crate namespace (e.g. `<crate>::{impl … PartialEq … for
        <crate>::Record}::ne` → `Record.Insts.PartialEq.ne`). These stub a
        `#[derive]`d method.
      * "aeneas-stdlib-prim" — a primitive aeneas does not model, owned by the Rust
        STANDARD LIBRARY (`core`/`alloc`/`std`/`proc_macro`/`test`): the item path
        is rooted in a stdlib crate (`core::slice::…first`, `core::option::…branch`).

    The owning-crate test (not a bare `.Insts.`/`{impl …}` shape match) is what
    keeps `core.option.Option.Insts.Try.branch` — a stdlib prim that aeneas names
    with an `.Insts.` segment AND whose path is a `{impl … for …}` trait-impl —
    correctly on the stdlib-prim side: its impl target lives in `core`, not in the
    crate. The `_node_name_from_item` derivation that maps an impl path to the
    `<Ty>.Insts.<…>` Lean name is reused so the crate-ownership test matches the
    same `<Ty>` the axiom name carries.
    """
    _STDLIB = ("core", "alloc", "std", "proc_macro", "test")
    # Root crate of the item path (`core::slice::…` → `core`,
    # `<crate>::{impl …}` → `<crate>`).
    root = item_path.split("::", 1)[0].strip() if item_path else ""
    impl = re.search(r"\bfor\s+([^}]+)\}", item_path)
    if impl is not None:
        # Trait impl: the OWNING crate is the impl-target type's crate.
        target = impl.group(1).strip()
        target_root = target.split("::", 1)[0].strip()
        if target_root and target_root not in _STDLIB:
            return "auto-derived-method"
        return "aeneas-stdlib-prim"
    # No trait-impl path. A stdlib-rooted item is a prim; anything else routed as a
    # bare boundary axiom (rare) defaults to stdlib-prim disclosure too — it is, by
    # construction, something aeneas did not model.
    if root in _STDLIB or not root:
        return "aeneas-stdlib-prim"
    # A crate-rooted bare axiom with no impl path: treat as a derived method only if
    # its name carries the aeneas derived-instance marker, else a crate-level prim.
    return "auto-derived-method" if ".Insts." in axiom_name else "aeneas-stdlib-prim"


def _boundary_axioms_from_block(block: List[str], namespace: str) -> List[BoundaryAxiom]:
    """Enumerate the disclosed boundary axioms in one axiom-only Preamble block.

    One `BoundaryAxiom` per `axiom` name, in source order — the deterministic
    TCB-disclosure unit. General: works for any target's axiom-only blocks.
    """
    item_path = _block_item_path(block)
    out: List[BoundaryAxiom] = []
    for name in _axiom_names(block):
        out.append(
            BoundaryAxiom(
                name=name,
                classification=_classify_boundary_axiom(item_path, name, namespace),
                stubs=_axiom_type_signature(block, name),
                item_path=item_path,
            )
        )
    return out


def _is_axiom_only_block(block: List[str]) -> bool:
    """True iff a block declares ONLY `axiom`s (no body-bearing declaration).

    An `axiom` has no `:=`/`by`/`where` body, so it cannot host the FILESPEC
    `-- BODY` marker and cannot be a tablet node (the kernel `validate_filespec`
    requires the line above `-- BODY` to end in `:=`/`by`/`where`). Such blocks
    are the disclosed Tier-B boundary axioms; they route to the Preamble (a free,
    un-pinned region with no `-- BODY` requirement) verbatim, so dependents reach
    them through `import Tablet.Preamble`. A block that ALSO carries a real
    `def`/`structure`/… is NOT axiom-only and stays a node.
    """
    return bool(_axiom_names(block)) and not _declared_names(block)


def _is_type_decl_block(block: List[str]) -> bool:
    """Data types share the Preamble with boundary axioms that depend on them."""
    heads = declaration_heads("\n".join(block))
    return bool(heads and heads[0].kind in {"structure", "inductive", "class"})


# A Lean identifier token: dotted-qualified names (`Tree.insert_in_opt_node`,
# `Node.rotate_left`) are single tokens. Exact declarations take precedence over
# prefixes when resolving field projections.
_IDENT_TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_']*)*")


def _strip_lean_comments(lines: List[str]) -> str:
    return mask_comments("\n".join(lines))


def _node_deps(decl_lines: List[str], self_node: str, decl_owner: Dict[str, str]) -> List[str]:
    """The sibling Tablet nodes this node's declaration text references.

    Deterministic: scan the decl region (CODE only — comments/docstrings stripped)
    for identifier tokens; for each token that is a declared name owned by a
    DIFFERENT node, record that owner. A dot-notation projection like `root.value`
    tokenizes as `root.value` (owner lookup misses — `root` is a local binder, not
    a declared name), while the type/decl reference `Node`/`Tree.insert_in_opt_node`
    tokenizes to a declared name and resolves. Returns a sorted, de-duplicated node
    list (topological determinism: aeneas emits in dependency order, but sorting
    makes the import block order independent of scan order).
    """
    seen = set()
    for tok in _IDENT_TOKEN.findall(_strip_lean_comments(decl_lines)):
        owner = decl_owner.get(tok)
        while owner is None and "." in tok:
            tok = tok.rsplit(".", 1)[0]
            owner = decl_owner.get(tok)
        if owner is not None and owner != self_node:
            seen.add(owner)
    return sorted(seen)


def _trim_surrounding_blank(region: str) -> str:
    """Drop leading/trailing blank lines; preserve interior verbatim.

    Mirrors the kernel `trim_surrounding_blank_lines` so the emitted config
    `lean` equals the kernel `prescribed_region` byte-for-byte (gate (c) is the
    authoritative re-check via the real kernel binary).
    """
    lines = region.split("\n")
    start = next((k for k, ln in enumerate(lines) if ln.strip() != ""), None)
    end = next((k for k in range(len(lines) - 1, -1, -1) if lines[k].strip() != ""), None)
    if start is None or end is None:
        return ""
    return "\n".join(lines[start : end + 1])


# ───────────────────── source fingerprints + config block ──────────────────


CRATE_SOURCE_TREE_SCHEMA = SOURCE_TREE_MANIFEST_SCHEMA


SOURCE_INPUT_IGNORED = frozenset({"target", ".git", ".lake", "__pycache__"})


def crate_source_tree_manifest(crate_dir: Path, *, excluded_names=(), allow_empty=False) -> dict:
    """Use the shared canonical source manifest for the selected input tree."""
    try:
        return source_tree_manifest(Path(crate_dir), excluded_names=excluded_names, allow_empty=allow_empty)
    except SourceTreeError as exc:
        raise SystemExit(str(exc)) from exc


def source_input_manifest(source: Path) -> dict:
    """Select the inputs copied for extraction, omitting build/repository metadata."""
    return crate_source_tree_manifest(source, excluded_names=SOURCE_INPUT_IGNORED, allow_empty=True)


def copy_source_inputs(source: Path, destination: Path) -> Path:
    """Freeze the selected input files and verify the complete copied tree."""
    manifest = source_input_manifest(source)
    destination.mkdir(parents=True, exist_ok=True)
    for item in manifest["files"]:
        relative = item["relative_path"]
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source / relative, target, follow_symlinks=False)
    if crate_source_tree_manifest(destination, allow_empty=True) != manifest:
        raise ValueError("Source snapshot differs from its selected input manifest")
    return destination


def crate_source_sha256(crate_dir: Path) -> str:
    """Hash the complete deterministic regular-file crate source manifest.

    One deliberately coarse tree hash is shared by every extracted model node,
    so any crate input change reopens every dependent model claim. Per-item
    fingerprints could reduce that blast radius later without weakening this
    complete-tree provenance boundary.
    """
    try:
        return source_tree_digest(Path(crate_dir))
    except SourceTreeError as exc:
        raise SystemExit(str(exc)) from exc


def crate_lib_source(crate_dir: Path) -> Path:
    """Resolve the crate's library source file from its Cargo.toml `[lib].path`."""
    cargo = crate_dir / "Cargo.toml"
    text = cargo.read_text(encoding="utf-8") if cargo.is_file() else ""
    m = re.search(r"^\s*\[lib\][^\[]*?^\s*path\s*=\s*\"([^\"]+)\"", text, re.MULTILINE | re.DOTALL)
    rel = m.group(1) if m else "src/lib.rs"
    src = (crate_dir / rel).resolve()
    if not src.is_file():
        # fall back to the conventional location
        alt = crate_dir / "src" / "lib.rs"
        if alt.is_file():
            return alt
        raise SystemExit(f"could not resolve crate library source (tried {src} and {alt})")
    return src


def build_pv_tablet_block(
    split: SplitResult,
    *,
    source_file_rel: str,
    source_sha256: str,
    extraction_toolchain: Dict[str, object],
    extraction_not_recorded: List[Dict[str, str]],
    node_prefix: str,
) -> dict:
    charon_version = str(extraction_toolchain.get("charon", ""))
    aeneas_version = str(extraction_toolchain.get("aeneas", ""))
    extractor_stack = [f"charon@{charon_version}", f"aeneas@{aeneas_version}"]
    toolchain_sha256 = pv_extraction_toolchain_sha256(
        extractor_stack, extraction_toolchain
    )
    # The namespace context the extractor emits for every model node: the
    # `namespace <crate>` line that scopes the principal declaration (see
    # `split_generated_lean` header build — `namespace {namespace}`, no closing
    # `end`, auto-closes at EOF). This is the part of the free preamble that
    # decides the fully-qualified name of the trusted decl, so the kernel pins it
    # as part of the node identity. `import`/`open`/`set_option` lines stay free.
    namespace_context = f"namespace {split.namespace}"
    models = []
    for node in split.nodes:
        # name-parity (kernel hard rule): the claiming node id, the prescribed
        # `name`, the on-disk `-- [TABLET NODE: <Name>]` marker, the file stem,
        # and the principal Lean decl name MUST all be identical. So `node` and
        # `name` are the bare generated decl name; the optional `--node-prefix`
        # only namespaces the challenge-TARGET registry id (a distinct key) so
        # multiple crates' models can coexist without an id clash.
        target_id = f"model:{node_prefix}{node.name}"
        models.append(
            {
                "id": target_id,
                "node": node.name,
                "name": node.name,
                "lean": node.region,
                "namespace_context": namespace_context,
                "model_dependencies": node.deps,
                "provenance": {
                    "problem_id": split.namespace,
                    "source_file": source_file_rel,
                    "source_sha256": source_sha256,
                    "extractor_toolchain_sha256": toolchain_sha256,
                },
            }
        )
    block = {
        "language": "rust",
        "verification_backend": "lean",
        "extractor_stack": extractor_stack,
        "extraction_toolchain": extraction_toolchain,
        "extraction_not_recorded": extraction_not_recorded,
        "extraction_models": models,
        "model_refinement": build_model_refinement_manifest(split),
    }
    return block


def pv_extraction_toolchain_sha256(
    extractor_stack: List[str], extraction_toolchain: Dict[str, object]
) -> str:
    """Match the kernel's canonical extraction-toolchain fingerprint exactly."""
    canonical = {
        "extraction_toolchain": extraction_toolchain,
        "extractor_stack": sorted(extractor_stack),
    }
    return hashlib.sha256(_canonical_json_bytes(canonical)).hexdigest()


def extraction_provenance_section(pv_tablet: dict) -> dict:
    """Project the extractor result into the manifest's evidence object."""
    digests = sorted(
        {
            (
                str((model.get("provenance") or {}).get("source_file", "")).strip(),
                str((model.get("provenance") or {}).get("source_sha256", "")).strip(),
            )
            for model in pv_tablet.get("extraction_models", [])
            if isinstance(model, dict)
        }
    )
    digests = [
        {"source_file": source_file, "source_sha256": source_sha256}
        for source_file, source_sha256 in digests
        if source_file and source_sha256
    ]
    stack = list(pv_tablet.get("extractor_stack") or [])
    toolchain = dict(pv_tablet.get("extraction_toolchain") or {})
    return {
        "extraction_toolchain": toolchain,
        "extractor_stack": stack,
        "extractor_toolchain_sha256": pv_extraction_toolchain_sha256(
            stack, toolchain
        ),
        "source_digests": digests,
        "not_recorded": list(pv_tablet.get("extraction_not_recorded") or []),
    }


def _canonical_json_bytes(value: object) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        allow_nan=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def build_model_refinement_manifest(split: SplitResult) -> dict:
    """Bind generated call paths to exact executable/opaque boundaries.

    Record the generated nodes and the reviewed or unresolved boundaries they
    name. The trust bootstrap determines which boundaries a goal depends on.
    """
    definition_names = [item["name"] for item in split.boundary_definitions]
    axiom_names = [item.name for item in split.boundary_axioms]
    nodes = []
    for node in split.nodes:
        nodes.append(
            {
                "node_id": node.name,
                "dependency_node_ids": sorted(node.deps),
                "reviewed_boundary_calls": [
                    name for name in definition_names if name in node.region
                ],
                "unresolved_boundary_calls": [
                    name for name in axiom_names if name in node.region
                ],
            }
        )
    nodes.sort(key=lambda item: item["node_id"].encode("utf-8"))
    manifest = {
        "schema": "trellis-aeneas-model-refinement/v1",
        "model_semantics": "trellis-aeneas-boundary-definitions/v1",
        "validity_definitions": [
            {
                **definition,
                "definition_sha256": hashlib.sha256(
                    definition["definition_source_utf8"].encode("utf-8")
                ).hexdigest(),
            }
            for definition in split.validity_definitions
        ],
        "boundary_definitions": split.boundary_definitions,
        "unresolved_boundary_axioms": [
            {
                "name": item.name,
                "stubs": item.stubs,
                "classification": item.classification,
            }
            for item in split.boundary_axioms
        ],
        "nodes": nodes,
    }
    manifest["manifest_sha256"] = hashlib.sha256(
        _canonical_json_bytes(manifest)
    ).hexdigest()
    return manifest


# ───────────────────────── TCB-disclosure manifest ─────────────────────────

# Manifest schema version. Bump when the `tcb_disclosure` shape changes so the
# parallel allowlist loader can adapt deterministically.
TCB_MANIFEST_SCHEMA = "pv-tcb-disclosure/v1"


def build_tcb_manifest(
    split: SplitResult, extraction_provenance: Optional[dict] = None
) -> dict:
    """The deterministic Tier-B TCB-disclosure manifest for a split.

    `setup_pv_repo.sh` derives the exact closure-gate allowlist from `global`.
    """
    disclosed_axioms = split.boundary_axioms
    names = sorted({ax.name for ax in disclosed_axioms})
    disclosure = [
        {
            "name": ax.name,
            "classification": ax.classification,
            "stubs": ax.stubs,
            "item_path": ax.item_path,
        }
        for ax in disclosed_axioms
    ]
    manifest = {
        # ── operator-readable provenance (loader ignores these object keys) ──
        "schema": TCB_MANIFEST_SCHEMA,
        "namespace": split.namespace,
        "tcb_disclosure": disclosure,
        "boundary_definitions": split.boundary_definitions,
        "validity_definitions": [
            {**definition, "definition_sha256": hashlib.sha256(
                definition["definition_source_utf8"].encode("utf-8")
            ).hexdigest()} for definition in split.validity_definitions
        ],
        # ── APPROVED_AXIOMS.json allowlist payload (loader-consumed) ──
        "global": names,
        "nodes": {},
    }
    if extraction_provenance is not None:
        manifest["extraction_provenance"] = extraction_provenance
    return manifest


def write_tcb_manifest(
    split: SplitResult,
    path: Path,
    extraction_provenance: Optional[dict] = None,
) -> dict:
    """Serialize the TCB manifest to `path` (stable JSON) and return the dict."""
    manifest = build_tcb_manifest(split, extraction_provenance)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return manifest


# ───────────────────────────── extract (one shot) ───────────────────────────


@dataclass
class ExtractOutput:
    out_dir: Path
    namespace: str
    nodes: List[str]
    pv_tablet: dict
    generated_sha256: str
    tcb_manifest: dict = field(default_factory=dict)
    run_evidence: Optional[PipelineRunEvidence] = None


def copy_crate_sources(crate_dir: Path, dest: Path, *, selected_inputs: bool = False) -> Path:
    """Copy the crate, using preparation's selected input manifest when requested."""
    crate_out = dest / "crate"
    if selected_inputs:
        copy_source_inputs(crate_dir, crate_out)
    else:
        shutil.copytree(crate_dir, crate_out, ignore=shutil.ignore_patterns("target", ".git"))
    if not (crate_out / "Cargo.toml").is_file():
        raise SystemExit(f"crate {crate_dir} has no Cargo.toml")
    return crate_out


def extract_once(
    crate_dir: Path,
    out_dir: Path,
    *,
    charon_bin: Path,
    aeneas_bin: Path,
    opam_switch: str,
    opam_env_json: Optional[Path] = None,
    cargo_target_dir: Path,
    work_root: Path,
    node_prefix: str,
    charon_version: str,
    aeneas_version: str,
    lean_version: str,
    model_boundary_contract: Optional[dict] = None,
    run_role: str = "primary",
    target_triple: Optional[str] = None,
    extraction_profile: str = "dev",
    strict_phase0: bool = False,
    extraction_config: Optional[dict] = None,
) -> ExtractOutput:
    if cargo_target_dir.exists():
        raise SystemExit(
            "refusing non-clean extraction target directory: "
            f"{cargo_target_dir}"
        )
    # Freeze the complete original crate tree before invoking either extractor.
    # This also rejects symlink/special-file inputs before an external tool can
    # observe a different tree from the one committed by provenance.
    if strict_phase0:
        crate_source_sha256(crate_dir)
    scratch = Path(tempfile.mkdtemp(prefix="extract.", dir=str(work_root)))
    run_evidence = None
    history_inputs = {}
    history_attempt = None
    if extraction_config and extraction_config.get("preserve_history"):
        from trellis.modeling.history import _tree, start_attempt
        history_inputs = _tree(crate_dir, "inputs/source", source=True)
        history_attempt = start_attempt(work_root / "extraction-history", "extractor-attempt",
                                        inputs=history_inputs, metadata={"output_path": str(scratch),
                                            "run_role": run_role, "extraction_config": extraction_config,
                                            "tool_identities": {"charon": charon_version, "aeneas": aeneas_version, "lean": lean_version}})
    try:
        env = build_extraction_env(charon_bin, opam_switch, opam_env_json)
        crate_src = copy_crate_sources(crate_dir, scratch, selected_inputs=extraction_config is not None)
        # Provenance commits every file in the frozen input actually extracted.
        source_sha = crate_source_sha256(crate_src)
        source_file_rel = crate_lib_source(crate_src).relative_to(crate_src.resolve()).as_posix()
        if extraction_config and extraction_config.get("preserve_history"):
            from trellis.modeling.history import _tree
            history_inputs = _tree(crate_src, "inputs/source", source=True)
        gen_out = scratch / "gen"
        generated_lean, llbc, run_evidence = run_pipeline(
            crate_src,
            gen_out,
            charon_bin=charon_bin,
            aeneas_bin=aeneas_bin,
            env=env,
            cargo_target_dir=cargo_target_dir,
            run_role=run_role,
            charon_version=charon_version,
            aeneas_version=aeneas_version,
            lean_version=lean_version,
            target_triple=target_triple,
            extraction_profile=extraction_profile,
            strict_phase0=strict_phase0,
            extraction_config=extraction_config,
        )
        generated_text = generated_lean.read_text(encoding="utf-8")
        generated_sha = hashlib.sha256(generated_text.encode("utf-8")).hexdigest()
        try:
            split = split_generated_lean(generated_text, model_boundary_contract)
        except SystemExit as exc:
            if isinstance(exc, PipelineStageFailure):
                raise
            raise _pipeline_failure(
                "generated_model_split",
                str(exc),
                run_role,
                run_evidence.steps,
            ) from exc
        # Materialize the Tablet/ model files.
        tablet = out_dir / "Tablet"
        tablet.mkdir(parents=True, exist_ok=True)
        # PV under-model assumptions: emit Preamble-tier
        # `Tablet/Assumptions.{lean,tex}` files. Workers stage paired Lean/NL
        # blocks here; ordinary NodeCorr gates that correspondence before the
        # assumptions lane sees the candidate. Human approval later updates only
        # the trust sinks (APPROVED_AXIOMS.json + tcb_manifest.json).
        #
        (tablet / f"{PREAMBLE_NODE}.lean").write_text(split.preamble_text, encoding="utf-8")
        (tablet / f"{ASSUMPTIONS_NODE}.lean").write_text(
            _empty_assumptions_lean(split.validity_definitions), encoding="utf-8"
        )
        (tablet / f"{ASSUMPTIONS_NODE}.tex").write_text(
            _empty_assumptions_tex(), encoding="utf-8"
        )
        for node in split.nodes:
            imports = f"import Tablet.{ASSUMPTIONS_NODE}\n" if split.validity_definitions else ""
            (tablet / f"{node.name}.lean").write_text(imports + node.text, encoding="utf-8")

        pv_block = build_pv_tablet_block(
            split,
            source_file_rel=source_file_rel,
            source_sha256=source_sha,
            extraction_toolchain=run_evidence.extraction_toolchain,
            extraction_not_recorded=run_evidence.not_recorded,
            node_prefix=node_prefix,
        )
        # Keep the raw generated Lean alongside for provenance/debugging.
        (out_dir / "generated.lean").write_text(generated_text, encoding="utf-8")
        (out_dir / "pv_tablet.json").write_text(json.dumps(pv_block, indent=2) + "\n", encoding="utf-8")
        # The deterministic Tier-B TCB-disclosure manifest (the routed boundary
        # axioms) — always emitted alongside, even when empty (montgomery). It is a
        # valid `APPROVED_AXIOMS.json` the kernel loader consumes as-is.
        provenance = extraction_provenance_section(pv_block)
        tcb_manifest = write_tcb_manifest(
            split,
            out_dir / "tcb_manifest.json",
            extraction_provenance=provenance,
        )
        run_evidence.extraction_provenance = provenance
        run_evidence.llbc_sha256 = hashlib.sha256(llbc.read_bytes()).hexdigest()
        run_evidence.generated_lean_sha256 = generated_sha
        run_evidence.tablet_root_sha256 = _dir_sha256(tablet)
        return ExtractOutput(
            out_dir=out_dir,
            namespace=split.namespace,
            nodes=[n.name for n in split.nodes],
            pv_tablet=pv_block,
            generated_sha256=generated_sha,
            tcb_manifest=tcb_manifest,
            run_evidence=run_evidence,
        )
    finally:
        if extraction_config and extraction_config.get("preserve_history"):
            from trellis.modeling.history import record_extractor_attempt
            failure = sys.exc_info()[1]
            steps = run_evidence.steps if run_evidence is not None else getattr(getattr(failure, "result", None), "steps", [])
            record_extractor_attempt(work_root, scratch, inputs=history_inputs, attempt_start_id=history_attempt, metadata={
                "run_role": run_role, "status": "failed" if failure else "succeeded",
                "error": str(failure) if failure else "",
                "steps": [_typed_step(step) for step in steps],
                "extraction_config": extraction_config,
                "tool_identities": {"charon": charon_version, "aeneas": aeneas_version, "lean": lean_version},
            })
        shutil.rmtree(scratch, ignore_errors=True)


# ──────────────────────────────── gates ────────────────────────────────────


def _toolchain_executable_for_cwd(
    tool: str, env: Dict[str, str], cwd: Path
) -> Path:
    """Return a real rustup-selected toolchain executable for ``cwd``.

    A rustup installation normally exposes ``cargo`` as a symlink to the
    multi-call ``rustup`` binary.  Resolving that symlink and then executing the
    result as ``rustup build ...`` changes the proxy dispatch and fails.  Ask
    rustup for the selected, toolchain-local Cargo binary instead.  Besides
    preserving the command's meaning, this gives the evidence closure a regular
    executable file to hash and copy rather than a mutable proxy symlink.
    """
    tool_on_path = shutil.which(tool, path=env["PATH"])
    if tool_on_path is None:
        raise SystemExit(f"{tool} is absent from the closed kernel-checker build PATH")
    selected = Path(tool_on_path).resolve(strict=True)
    if selected.name == "rustup":
        lookup = subprocess.run(
            [str(selected), "which", tool],
            cwd=str(cwd),
            env=env,
            check=False,
            capture_output=True,
        )
        if lookup.returncode != 0:
            raise SystemExit(
                f"rustup could not resolve the {tool} executable selected for the "
                "kernel checker build:\n"
                + lookup.stdout.decode("utf-8", errors="replace")
                + lookup.stderr.decode("utf-8", errors="replace")
            )
        selected_text = lookup.stdout.decode("utf-8", errors="strict").strip()
        if not selected_text or "\n" in selected_text:
            raise SystemExit(f"rustup which {tool} returned an invalid executable path")
        selected = Path(selected_text).resolve(strict=True)
    if not selected.is_file() or selected.is_symlink() or not os.access(selected, os.X_OK):
        raise SystemExit(f"selected {tool} is not an executable regular file: {selected}")
    return selected


def _cargo_executable_for_cwd(env: Dict[str, str], cwd: Path) -> Path:
    return _toolchain_executable_for_cwd("cargo", env, cwd)


def _kernel_example_bin(
    kernel_manifest: Path,
    cargo_target_dir: Path,
    name: str,
    *,
    phase0_failure_steps: Optional[List[PipelineStepEvidence]] = None,
) -> Tuple[Path, PipelineStepEvidence]:
    """Build and evidence one real-kernel extraction checker binary."""
    env = _closed_host_tool_env()
    env["CARGO_TARGET_DIR"] = str(cargo_target_dir)
    cargo_bin = _cargo_executable_for_cwd(env, kernel_manifest.parent)
    # Invoking the selected Cargo binary directly avoids proxy ambiguity; bind
    # its rustc explicitly so Cargo does not fall back through a floating PATH
    # proxy when it spawns compilation jobs.
    env["RUSTC"] = str(
        _toolchain_executable_for_cwd("rustc", env, kernel_manifest.parent)
    )
    argv = [
        str(cargo_bin),
        "build",
        "-j4",
        "--locked",
        "--offline",
        "--manifest-path",
        str(kernel_manifest),
        "--example",
        name,
    ]
    built = run_compute(
        argv,
        cwd=str(kernel_manifest.parent),
        env=env,
        check=False,
        capture_output=True,
    )
    evidence = PipelineStepEvidence(
        tool="cargo-build-kernel-example",
        argv=argv,
        cwd=str(kernel_manifest.parent),
        effective_environment=dict(sorted(env.items())),
        exit_status=built.returncode,
        stdout=built.stdout,
        stderr=built.stderr,
    )
    if built.returncode != 0:
        detail = (
            f"building kernel checker {name} failed with status {built.returncode}:\n"
            + built.stdout.decode("utf-8", errors="replace")
            + built.stderr.decode("utf-8", errors="replace")
        )
        if phase0_failure_steps is not None:
            phase0_failure_steps.append(evidence)
            raise _pipeline_failure(
                "filespec_region_gates", detail, "primary", phase0_failure_steps
            )
        raise SystemExit(detail)
    cand = cargo_target_dir / "debug" / "examples" / name
    if not cand.is_file():
        raise SystemExit(f"kernel example binary not found after build: {cand}")
    return cand, evidence


def _prebuilt_kernel_checker(path: Path, label: str) -> Path:
    """Resolve one caller-supplied trust-kernel gate executable."""
    try:
        mode = path.lstat().st_mode
        resolved = path.resolve(strict=True)
    except OSError as exc:
        raise SystemExit(f"prebuilt {label} checker is unavailable: {path}: {exc}") from exc
    if path.is_symlink() or not stat.S_ISREG(mode) or not os.access(resolved, os.X_OK):
        raise SystemExit(
            f"prebuilt {label} checker is not an executable non-symlink regular file: {path}"
        )
    return resolved


def run_gates(
    crate_dir: Path,
    out_dir: Path,
    *,
    charon_bin: Path,
    aeneas_bin: Path,
    opam_switch: str,
    opam_env_json: Optional[Path] = None,
    cargo_target_dir: Path,
    work_root: Path,
    node_prefix: str,
    kernel_manifest: Path,
    charon_version: str,
    aeneas_version: str,
    lean_version: str,
    filespec_checker: Optional[Path] = None,
    prescribed_region_checker: Optional[Path] = None,
    target_triple: Optional[str] = None,
    extraction_profile: str = "dev",
    strict_phase0: bool = False,
    model_boundary_contract: Optional[dict] = None,
    extraction_config: Optional[dict] = None,
) -> ExtractOutput:
    """Extract + run gates (a)/(b)/(c). Returns the primary ExtractOutput."""
    # The primary extraction (the artifact we keep).
    primary = extract_once(
        crate_dir,
        out_dir,
        charon_bin=charon_bin,
        aeneas_bin=aeneas_bin,
        opam_switch=opam_switch,
        opam_env_json=opam_env_json,
        cargo_target_dir=cargo_target_dir / "extractA",
        work_root=work_root,
        node_prefix=node_prefix,
        charon_version=charon_version,
        aeneas_version=aeneas_version,
        lean_version=lean_version,
        run_role="primary",
        model_boundary_contract=model_boundary_contract,
        target_triple=target_triple,
        extraction_profile=extraction_profile,
        strict_phase0=strict_phase0,
        extraction_config=extraction_config,
    )

    # GATE (a): a second, fully-clean extraction -> byte-identical Model files.
    out_b = work_root / "gate_b_out"
    if out_b.exists():
        shutil.rmtree(out_b)
    try:
        second = extract_once(
            crate_dir,
            out_b,
            charon_bin=charon_bin,
            aeneas_bin=aeneas_bin,
            opam_switch=opam_switch,
            opam_env_json=opam_env_json,
            cargo_target_dir=cargo_target_dir / "extractB",
            work_root=work_root,
            node_prefix=node_prefix,
            charon_version=charon_version,
            aeneas_version=aeneas_version,
            lean_version=lean_version,
            run_role="determinism_replay",
            target_triple=target_triple,
            extraction_profile=extraction_profile,
            strict_phase0=strict_phase0,
            model_boundary_contract=model_boundary_contract,
            extraction_config=extraction_config,
        )
    except PipelineStageFailure as exc:
        if strict_phase0 and primary.run_evidence is not None:
            raise _pipeline_failure(
                "second_clean_extraction",
                exc.result.detail,
                "determinism_replay",
                [*primary.run_evidence.steps, *exc.result.steps],
            ) from exc
        raise
    diffs = _diff_tablet_dirs(out_dir / "Tablet", out_b / "Tablet")
    if diffs:
        detail = "GATE (a) FAILED — A/B Model files differ:\n  " + "\n  ".join(diffs)
        if strict_phase0 and primary.run_evidence is not None and second.run_evidence is not None:
            raise _pipeline_failure(
                "second_clean_extraction",
                detail,
                "determinism_replay",
                [*primary.run_evidence.steps, *second.run_evidence.steps],
            )
        raise SystemExit(detail)
    sha_a = _dir_sha256(out_dir / "Tablet")
    sha_b = _dir_sha256(out_b / "Tablet")
    if sha_a != sha_b:
        detail = f"GATE (a) FAILED — Tablet aggregate sha differs: {sha_a} != {sha_b}"
        if strict_phase0 and primary.run_evidence is not None and second.run_evidence is not None:
            raise _pipeline_failure(
                "second_clean_extraction",
                detail,
                "determinism_replay",
                [*primary.run_evidence.steps, *second.run_evidence.steps],
            )
        raise SystemExit(detail)
    log(f"GATE (a) PASS: A/B Model files byte-identical (Tablet aggregate sha256={sha_a})")
    log(f"           generated-Lean sha256 A={primary.generated_sha256} B={second.generated_sha256}")
    if primary.run_evidence is None or second.run_evidence is None:
        raise SystemExit("internal error: extraction process evidence is absent")
    gate_env = _closed_host_tool_env()

    # GATE (b): every Model file passes validate_filespec (real kernel).
    if filespec_checker is None:
        scan_bin, scan_build_evidence = _kernel_example_bin(
            kernel_manifest,
            cargo_target_dir,
            "scan_tablet_filespec",
            phase0_failure_steps=(
                primary.run_evidence.steps if strict_phase0 else None
            ),
        )
        primary.run_evidence.steps.append(scan_build_evidence)
    else:
        scan_bin = _prebuilt_kernel_checker(filespec_checker, "filespec")
    scan_argv = [str(scan_bin), str(out_dir)]
    scan = run_compute(
        scan_argv,
        cwd=str(kernel_manifest.parent),
        env=gate_env,
        capture_output=True,
    )
    primary.run_evidence.steps.append(
        PipelineStepEvidence(
            tool="kernel-scan-tablet-filespec",
            argv=scan_argv,
            cwd=str(kernel_manifest.parent),
            effective_environment=dict(sorted(gate_env.items())),
            exit_status=scan.returncode,
            stdout=scan.stdout,
            stderr=scan.stderr,
        )
    )
    scan_stdout = scan.stdout.decode("utf-8", errors="replace")
    scan_stderr = scan.stderr.decode("utf-8", errors="replace")
    log("GATE (b) scan_tablet_filespec:\n" + scan_stdout.rstrip())
    if scan.returncode != 0:
        detail = (
            f"GATE (b) FAILED — filespec offenders:\n{scan_stdout}\n{scan_stderr}"
        )
        if strict_phase0:
            raise _pipeline_failure(
                "filespec_region_gates",
                detail,
                "primary",
                primary.run_evidence.steps,
            )
        raise SystemExit(detail)
    log("GATE (b) PASS: all Model files pass validate_filespec (one TABLET-NODE + one -- BODY, delimiter ok)")

    # GATE (c): emitted `lean` == prescribed_region(include_body) (real kernel).
    if prescribed_region_checker is None:
        pr_bin, pr_build_evidence = _kernel_example_bin(
            kernel_manifest,
            cargo_target_dir,
            "print_prescribed_region",
            phase0_failure_steps=(
                primary.run_evidence.steps if strict_phase0 else None
            ),
        )
        primary.run_evidence.steps.append(pr_build_evidence)
    else:
        pr_bin = _prebuilt_kernel_checker(
            prescribed_region_checker, "prescribed-region"
        )
    # `node` is the BARE generated decl name (name-parity invariant): only the
    # target `id` carries `node_prefix`, never `node`/`name`. So key on `node`
    # directly — stripping `node_prefix` here would spuriously mangle any node
    # whose bare name happens to begin with the prefix string (file lookup then
    # fails with "No such file").
    model_by_node = {m["node"]: m["lean"] for m in primary.pv_tablet["extraction_models"]}
    mismatches = []
    for node, expected in model_by_node.items():
        node_file = out_dir / "Tablet" / f"{node}.lean"
        pr_argv = [str(pr_bin), str(node_file)]
        got = run_compute(
            pr_argv,
            cwd=str(kernel_manifest.parent),
            env=gate_env,
            capture_output=True,
        )
        primary.run_evidence.steps.append(
            PipelineStepEvidence(
                tool="kernel-print-prescribed-region",
                argv=pr_argv,
                cwd=str(kernel_manifest.parent),
                effective_environment=dict(sorted(gate_env.items())),
                exit_status=got.returncode,
                stdout=got.stdout,
                stderr=got.stderr,
            )
        )
        got_stdout = got.stdout.decode("utf-8", errors="replace")
        got_stderr = got.stderr.decode("utf-8", errors="replace")
        if got.returncode != 0:
            mismatches.append(f"{node}: prescribed_region failed: {got_stderr.strip()}")
            continue
        if got_stdout != expected:
            mismatches.append(
                f"{node}: emitted `lean` != kernel prescribed_region\n"
                f"    --- emitted ---\n{expected}\n    --- kernel ---\n{got_stdout}"
            )
    if mismatches:
        detail = "GATE (c) FAILED — emitted lean != prescribed_region:\n" + "\n".join(mismatches)
        if strict_phase0:
            raise _pipeline_failure(
                "filespec_region_gates",
                detail,
                "primary",
                primary.run_evidence.steps,
            )
        raise SystemExit(detail)
    log(f"GATE (c) PASS: all {len(model_by_node)} emitted `lean` texts byte-equal the kernel prescribed_region")

    # GATE (c2): the pinned `namespace_context` round-trips — the kernel pins
    # this as part of the model node's identity and byte-checks the node file's
    # preamble namespace context against it at every acceptance
    # (`challenge_conformance_errors`). Assert each emitted node file's preamble
    # namespace context (the `namespace …`/`end …` lines above the
    # `-- [TABLET NODE]` marker — exactly the kernel's
    # `filespec_split::namespace_context`) equals the pinned value, so a strip
    # IS rejected while an added import/open is NOT.
    ctx_by_node = {
        m["node"]: m["namespace_context"]
        for m in primary.pv_tablet["extraction_models"]
    }
    ctx_mismatches = []
    for node, pinned in ctx_by_node.items():
        node_file = out_dir / "Tablet" / f"{node}.lean"
        actual = _file_namespace_context(node_file.read_text(encoding="utf-8"))
        if actual != pinned:
            ctx_mismatches.append(
                f"{node}: emitted namespace_context != file preamble context\n"
                f"    pinned={pinned!r} actual={actual!r}"
            )
    if ctx_mismatches:
        detail = (
            "GATE (c2) FAILED — namespace_context does not round-trip:\n" + "\n".join(ctx_mismatches)
        )
        if strict_phase0:
            raise _pipeline_failure(
                "filespec_region_gates",
                detail,
                "primary",
                primary.run_evidence.steps,
            )
        raise SystemExit(detail)
    log(f"GATE (c2) PASS: all {len(ctx_by_node)} pinned namespace_context values round-trip")

    write_extraction_evidence(
        out_dir,
        [primary.run_evidence, second.run_evidence],
        determinism_status="passed",
    )

    shutil.rmtree(out_b, ignore_errors=True)
    return primary


def _file_namespace_context(content: str) -> str:
    """The namespace context of a node file: the `namespace …`/`end …` lines in
    the PREAMBLE (above the `-- [TABLET NODE: …]` marker), joined by `\\n`.

    Mirrors the kernel `filespec_split::namespace_context`: scope lines only,
    excluding `import`/`open`/`set_option`/comments, and only the preamble.
    """
    lines = content.split("\n")
    marker_idx = next(
        (i for i, ln in enumerate(lines) if ln.strip().startswith("-- [TABLET NODE:") and ln.strip().endswith("]")),
        len(lines),
    )
    ctx = [
        ln.rstrip("\r")
        for ln in lines[:marker_idx]
        if ln.lstrip().startswith("namespace ")
        or ln.strip() == "namespace"
        or ln.lstrip().startswith("end ")
        or ln.strip() == "end"
    ]
    return "\n".join(ctx).strip()


def _dir_sha256(tablet_dir: Path) -> str:
    h = hashlib.sha256()
    for path in sorted(tablet_dir.glob("*.lean")):
        h.update(path.name.encode("utf-8"))
        h.update(b"\0")
        h.update(path.read_bytes())
        h.update(b"\0")
    return h.hexdigest()


def _diff_tablet_dirs(a: Path, b: Path) -> List[str]:
    out: List[str] = []
    names_a = {p.name for p in a.glob("*.lean")}
    names_b = {p.name for p in b.glob("*.lean")}
    for name in sorted(names_a ^ names_b):
        out.append(f"only in one run: {name}")
    for name in sorted(names_a & names_b):
        if (a / name).read_bytes() != (b / name).read_bytes():
            out.append(f"differs: {name}")
    return out


def write_extraction_evidence(
    out_dir: Path,
    runs: List[PipelineRunEvidence],
    *,
    determinism_status: str,
) -> None:
    """Write the complete process and A/B extraction evidence closure.

    STDOUT.bin and STDERR.bin are the byte concatenation of each run/step in
    COMMAND.json order. RESULT.json supplies exact offsets, lengths, hashes,
    and exit statuses, so no text decoding or delimiter convention can alter a
    tool's raw output.
    """
    if determinism_status not in {"passed", "not_run"}:
        raise ValueError("invalid extraction determinism status")
    if not runs or (determinism_status == "passed" and len(runs) != 2):
        raise ValueError("extraction evidence has an invalid run count")
    if any(not run.extraction_provenance for run in runs):
        raise ValueError("extraction evidence lacks extraction provenance")
    if any(
        run.extraction_provenance != runs[0].extraction_provenance
        for run in runs[1:]
    ):
        raise ValueError("A/B extraction provenance differs")
    command_runs = []
    result_runs = []
    stdout = bytearray()
    stderr = bytearray()
    for run in runs:
        command_steps = []
        result_steps = []
        for step in run.steps:
            stdout_offset = len(stdout)
            stderr_offset = len(stderr)
            stdout.extend(step.stdout)
            stderr.extend(step.stderr)
            command_steps.append(
                {
                    "tool": step.tool,
                    "argv": step.argv,
                    "working_directory": step.cwd,
                    "effective_environment": step.effective_environment,
                }
            )
            result_steps.append(
                {
                    "tool": step.tool,
                    "exit_status": step.exit_status,
                    "stdout_offset": stdout_offset,
                    "stdout_byte_length": len(step.stdout),
                    "stdout_sha256": hashlib.sha256(step.stdout).hexdigest(),
                    "stderr_offset": stderr_offset,
                    "stderr_byte_length": len(step.stderr),
                    "stderr_sha256": hashlib.sha256(step.stderr).hexdigest(),
                }
            )
        command_runs.append(
            {
                "run_role": run.run_role,
                "steps": command_steps,
            }
        )
        result_runs.append(
            {
                "run_role": run.run_role,
                "steps": result_steps,
                "generated_lean_sha256": run.generated_lean_sha256,
                "tablet_root_sha256": run.tablet_root_sha256,
            }
        )

    tool_source_checkouts = []
    kernel_gate_checkers = []
    if determinism_status == "passed":
        first_steps = {step.tool: step for step in runs[0].steps}
        for tool in (
            "kernel-scan-tablet-filespec",
            "kernel-print-prescribed-region",
        ):
            step = first_steps.get(tool)
            if step is None or not step.argv:
                raise ValueError(f"missing {tool} gate command")
            executable = Path(step.argv[0]).resolve(strict=True)
            kernel_gate_checkers.append(
                {
                    "tool": tool,
                    "path": str(executable),
                    "sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
                }
            )
        for tool in ("charon", "aeneas"):
            step = first_steps.get(tool)
            if step is None or not step.argv:
                raise ValueError(f"missing {tool} extraction command")
            executable = Path(step.argv[0]).resolve(strict=True)
            git_env = _closed_host_tool_env()

            def git_output(*args: str) -> str:
                completed = subprocess.run(
                    ["git", "-C", str(executable.parent), *args],
                    env=git_env,
                    check=False,
                    capture_output=True,
                    text=True,
                )
                if completed.returncode != 0:
                    raise ValueError(
                        f"cannot resolve clean git provenance for {tool}: "
                        f"{completed.stderr.strip()}"
                    )
                return completed.stdout.strip()

            repository_root = Path(
                git_output("rev-parse", "--show-toplevel")
            ).resolve(strict=True)
            try:
                executable_relative = executable.relative_to(repository_root)
            except ValueError as error:
                raise ValueError(
                    f"{tool} executable is outside its source checkout"
                ) from error
            dirty = git_output("status", "--porcelain", "--untracked-files=all")
            if dirty:
                raise ValueError(f"{tool} source checkout is dirty")
            tool_source_checkouts.append(
                {
                    "tool": tool,
                    "repository_root": str(repository_root),
                    "commit": git_output("rev-parse", "HEAD"),
                    "origin": git_output("remote", "get-url", "origin"),
                    "executable_relative_path": executable_relative.as_posix(),
                    "clean_worktree": True,
                }
            )

    command = {
        "schema": "trellis-extraction-command/v1",
        "extractor_script_path": str(EXTRACTOR_SCRIPT_PATH),
        "extractor_script_sha256": EXTRACTOR_SCRIPT_SHA256,
        "extraction_provenance": runs[0].extraction_provenance,
        "kernel_gate_checkers": kernel_gate_checkers,
        "declaration_reader_sha256": hashlib.sha256(
            (EXTRACTOR_SCRIPT_PATH.parent.parent / "trellis/lean_declarations.py").read_bytes()
        ).hexdigest(),
        "log_concatenation_order": "runs_then_steps_in_array_order",
        "tool_source_checkouts": tool_source_checkouts,
        "runs": command_runs,
    }
    command_bytes = _canonical_json_bytes(command)
    stdout_bytes = bytes(stdout)
    stderr_bytes = bytes(stderr)
    result = {
        "schema": "trellis-extraction-result/v2",
        "command_sha256": hashlib.sha256(command_bytes).hexdigest(),
        "stdout_sha256": hashlib.sha256(stdout_bytes).hexdigest(),
        "stdout_byte_length": len(stdout_bytes),
        "stderr_sha256": hashlib.sha256(stderr_bytes).hexdigest(),
        "stderr_byte_length": len(stderr_bytes),
        "runs": result_runs,
    }
    result_bytes = _canonical_json_bytes(result)
    determinism = {
        "schema": "trellis-extraction-determinism/v2",
        "status": determinism_status,
        "comparison": "generated_lean_and_complete_tablet_tree_byte_identity",
        "kernel_filespec_gate": (
            "passed" if determinism_status == "passed" else "not_run"
        ),
        "kernel_prescribed_region_gate": (
            "passed" if determinism_status == "passed" else "not_run"
        ),
        "namespace_context_roundtrip_gate": (
            "passed" if determinism_status == "passed" else "not_run"
        ),
        "run_roles": [run.run_role for run in runs],
        "generated_lean_sha256": [run.generated_lean_sha256 for run in runs],
        "tablet_root_sha256": [run.tablet_root_sha256 for run in runs],
        "generated_lean_byte_identical": (
            len(runs) == 2
            and runs[0].generated_lean_sha256 == runs[1].generated_lean_sha256
        ),
        "tablet_tree_byte_identical": (
            len(runs) == 2
            and runs[0].tablet_root_sha256 == runs[1].tablet_root_sha256
        ),
        "result_sha256": hashlib.sha256(result_bytes).hexdigest(),
    }
    if determinism_status == "passed" and not (
        determinism["generated_lean_byte_identical"]
        and determinism["tablet_tree_byte_identical"]
    ):
        raise ValueError("cannot emit a passed extraction determinism receipt")

    evidence = out_dir / "extraction-evidence"
    evidence.mkdir(parents=True, exist_ok=False)
    (evidence / "COMMAND.json").write_bytes(command_bytes)
    (evidence / "STDOUT.bin").write_bytes(stdout_bytes)
    (evidence / "STDERR.bin").write_bytes(stderr_bytes)
    (evidence / "RESULT.json").write_bytes(result_bytes)
    (evidence / "DETERMINISM.json").write_bytes(
        _canonical_json_bytes(determinism)
    )


# ──────────────────────────────── main ─────────────────────────────────────


def main(argv: Optional[List[str]] = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--crate", required=True, type=Path, help="Rust crate directory (with Cargo.toml)")
    ap.add_argument("--out", required=True, type=Path, help="output dir for Tablet/ + pv_tablet.json")
    ap.add_argument("--node-prefix", default="", help="prefix prepended to every node id/name")
    ap.add_argument("--charon-bin", type=Path, default=DEFAULT_CHARON_BIN)
    ap.add_argument("--aeneas-bin", type=Path, default=DEFAULT_AENEAS_BIN)
    ap.add_argument("--opam-switch", default=DEFAULT_OPAM_SWITCH)
    ap.add_argument(
        "--opam-env-json",
        type=Path,
        default=None,
        help="canonical pre-resolved switch environment; suppresses opam invocation",
    )
    ap.add_argument("--lean-version", default=DEFAULT_LEAN_VERSION)
    ap.add_argument("--charon-version", default=DEFAULT_CHARON_VERSION)
    ap.add_argument("--aeneas-version", default=DEFAULT_AENEAS_VERSION)
    ap.add_argument(
        "--target",
        default=None,
        help="Rust target triple (default: the host reported by pinned rustc -vV)",
    )
    ap.add_argument(
        "--extraction-profile",
        default="dev",
        help="Cargo profile used by Charon and the effective-cfg probe (default: dev)",
    )
    ap.add_argument("--kernel-manifest", type=Path, default=None, help="kernel Cargo.toml (for --gate)")
    ap.add_argument(
        "--filespec-checker",
        type=Path,
        default=None,
        help="prebuilt scan_tablet_filespec executable (suppresses its Cargo build)",
    )
    ap.add_argument(
        "--prescribed-region-checker",
        type=Path,
        default=None,
        help="prebuilt print_prescribed_region executable (suppresses its Cargo build)",
    )
    ap.add_argument("--cargo-target-dir", type=Path, default=None, help="isolated CARGO_TARGET_DIR")
    ap.add_argument("--work-root", type=Path, default=None, help="scratch root (reaped)")
    ap.add_argument("--gate", action="store_true", help="run determinism + filespec + region gates")
    ap.add_argument("--model-boundary-contract", type=Path, default=None)
    ap.add_argument("--extraction-config", type=Path, help="JSON Cargo, Charon, and Aeneas argument lists")
    ap.add_argument(
        "--phase0-result",
        type=Path,
        default=None,
        help=argparse.SUPPRESS,
    )
    ap.add_argument(
        "--emit-tcb-manifest",
        type=Path,
        default=None,
        metavar="PATH",
        help=(
            "also write the Tier-B TCB-disclosure manifest (deterministically "
            "enumerated boundary axioms, each with name, classification, and the "
            "type it stubs) to PATH. The canonical "
            "copy is always written to <out>/tcb_manifest.json."
        ),
    )
    args = ap.parse_args(argv)
    model_boundary_contract = load_model_boundary_contract(args.model_boundary_contract)
    extraction_config = None
    if args.extraction_config is not None:
        supplied_extraction = json.loads(args.extraction_config.read_text())
        extraction_config = extraction_arguments(supplied_extraction)
        if supplied_extraction.get("preserve_history") is True:
            extraction_config["preserve_history"] = True
        if "source_tools" in supplied_extraction:
            if not isinstance(supplied_extraction["source_tools"], dict):
                raise SystemExit("extraction source_tools must be an object")
            extraction_config["source_tools"] = supplied_extraction["source_tools"]

    crate = args.crate.resolve()
    out_dir = args.out.resolve()
    if not (crate / "Cargo.toml").is_file():
        raise SystemExit(f"--crate has no Cargo.toml: {crate}")
    for tool in (args.charon_bin, args.aeneas_bin):
        if not Path(tool).is_file():
            raise SystemExit(f"tool not found: {tool}")

    script_root = Path(__file__).resolve().parent.parent
    kernel_manifest = (args.kernel_manifest or (script_root / "kernel" / "Cargo.toml")).resolve()
    cargo_target_dir = (args.cargo_target_dir or (script_root / ".petarget")).resolve()
    work_root = (args.work_root or (script_root / ".work" / "extract")).resolve()
    work_root.mkdir(parents=True, exist_ok=True)
    out_dir.mkdir(parents=True, exist_ok=True)
    opam_env_json = (
        args.opam_env_json.resolve(strict=True)
        if args.opam_env_json is not None
        else None
    )

    if args.gate:
        result = run_gates(
            crate,
            out_dir,
            charon_bin=args.charon_bin.resolve(),
            aeneas_bin=args.aeneas_bin.resolve(),
            opam_switch=args.opam_switch,
            opam_env_json=opam_env_json,
            cargo_target_dir=cargo_target_dir,
            work_root=work_root,
            node_prefix=args.node_prefix,
            kernel_manifest=kernel_manifest,
            filespec_checker=args.filespec_checker,
            prescribed_region_checker=args.prescribed_region_checker,
            charon_version=args.charon_version,
            aeneas_version=args.aeneas_version,
            lean_version=args.lean_version,
            target_triple=args.target,
            extraction_profile=args.extraction_profile,
            strict_phase0=args.phase0_result is not None,
            model_boundary_contract=model_boundary_contract,
            extraction_config=extraction_config,
        )
    else:
        result = extract_once(
            crate,
            out_dir,
            charon_bin=args.charon_bin.resolve(),
            aeneas_bin=args.aeneas_bin.resolve(),
            opam_switch=args.opam_switch,
            opam_env_json=opam_env_json,
            cargo_target_dir=cargo_target_dir / "extract",
            work_root=work_root,
            node_prefix=args.node_prefix,
            charon_version=args.charon_version,
            aeneas_version=args.aeneas_version,
            lean_version=args.lean_version,
            target_triple=args.target,
            extraction_profile=args.extraction_profile,
            strict_phase0=args.phase0_result is not None,
            model_boundary_contract=model_boundary_contract,
            extraction_config=extraction_config,
        )
        write_extraction_evidence(
            out_dir,
            [result.run_evidence],
            determinism_status="not_run",
        )

    # Optionally write an extra copy of the TCB-disclosure manifest where the
    # parallel allowlist consumes it (the canonical copy is <out>/tcb_manifest.json,
    # already written by extract_once). The split is re-read from the on-disk
    # generated Lean so this works for both --gate and plain runs.
    tcb_axioms = result.tcb_manifest.get("global", [])
    tcb_boundary_axioms = [
        d.get("name")
        for d in result.tcb_manifest.get("tcb_disclosure", [])
        if isinstance(d, dict)
    ]
    if args.emit_tcb_manifest is not None:
        split = split_generated_lean((out_dir / "generated.lean").read_text(encoding="utf-8"))
        write_tcb_manifest(
            split,
            args.emit_tcb_manifest.resolve(),
            extraction_provenance=result.tcb_manifest.get("extraction_provenance"),
        )

    log("")
    log(f"Extraction complete: {len(result.nodes)} model node(s) from crate {crate.name}")
    log(f"  namespace:        {result.namespace}")
    log(f"  nodes:            {', '.join(result.nodes)}")
    log(f"  generated sha:    {result.generated_sha256}")
    log(f"  Tablet/ dir:      {out_dir / 'Tablet'}")
    log(f"  pv_tablet.json:   {out_dir / 'pv_tablet.json'}")
    log(f"  tcb_manifest:     {out_dir / 'tcb_manifest.json'} ({len(tcb_axioms)} TCB axiom(s))")
    if args.emit_tcb_manifest is not None:
        log(f"  + tcb copy:       {args.emit_tcb_manifest.resolve()}")
    # Emit a small machine-readable summary on stdout for callers (setup script).
    print(
        json.dumps(
            {
                "namespace": result.namespace,
                "nodes": result.nodes,
                "generated_sha256": result.generated_sha256,
                "out_dir": str(out_dir),
                "tcb_axioms": tcb_axioms,
                "tcb_boundary_axioms": tcb_boundary_axioms,
            }
        )
    )
    if args.phase0_result is not None:
        _write_typed_result(
            args.phase0_result.resolve(),
            {
                "schema": EXTRACTOR_TYPED_RESULT_SCHEMA,
                "status": "passed",
                "first_failing_stage": None,
                "detail": "",
                "run_role": result.run_evidence.run_role,
                "steps": [_typed_step(step) for step in result.run_evidence.steps],
                "generated_lean_sha256": result.run_evidence.generated_lean_sha256,
                "llbc_sha256": result.run_evidence.llbc_sha256,
                "tablet_root_sha256": result.run_evidence.tablet_root_sha256,
            },
        )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except PipelineStageFailure as exc:
        try:
            index = sys.argv.index("--phase0-result")
            typed_path = Path(sys.argv[index + 1]).resolve()
        except (ValueError, IndexError):
            raise
        _write_typed_result(
            typed_path,
            {
                "schema": EXTRACTOR_TYPED_RESULT_SCHEMA,
                "status": "failed",
                "first_failing_stage": exc.result.stage,
                "detail": exc.result.detail,
                "run_role": exc.result.run_role,
                "steps": [_typed_step(step) for step in exc.result.steps],
                "generated_lean_sha256": "",
                "llbc_sha256": "",
                "tablet_root_sha256": "",
            },
        )
        print(exc.result.detail, file=sys.stderr)
        raise SystemExit(1)
