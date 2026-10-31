#!/bin/bash
# Prepare a program-verification (PV) tablet repo from a Rust crate. The PV
# sibling of setup_repo.sh / setup_revision_repo.sh — but PV has NO paper
# (Faithfulness is structurally absent: no paper_tex_path / main_result_*).
#
# This script:
#   1. runs scripts/extract_pv_model.py on --crate (charon -> aeneas -> single
#      Lean -> FILESPEC-split into Tablet/Preamble.lean + Tablet/<Node>.lean,
#      one node per top-level def / loop-trio / mutual block) and emits the
#      `pv_tablet` config block;
#   2. scaffolds the repo like setup_repo.sh (Lean lakefile + lean-toolchain,
#      APPROVED_AXIOMS.json, the PV canonical verifier rubrics (SOUNDNESS +
#      SUBSTANTIVENESS PV variants + neutral CORRESPONDENCE; no FAITHFULNESS /
#      DEVIATIONS), FILESPEC.md,
#      HUMAN_INPUT.md / INPUT_REQUEST.md, .trellis/ substructure, the Tablet
#      support sync) — but pins the Aeneas-support toolchain (rc2) and ADDS the
#      Aeneas `require` to the lakefile so the generated model imports resolve;
#   3. writes trellis.config.json from the example template with the `pv_tablet`
#      block spliced in and paper_tex_path / main_result_* OMITTED;
#   4. copies the authored GOAL.md verbatim and records the public functions
#      declared by its exact level-three headings; goal statements and support
#      nodes are authored during the campaign;
#   5. normally inits the runtime state via the kernel CLI `init_from_config`, which
#      reads the `pv_tablet` block and seeds node_role=ExtractionModel +
#      configured_challenge_targets (with source_sha256 + the stamped
#      extractor_toolchain_sha256) + the approved_extraction_provenance baseline.
#      `--defer-runtime-init` stops before this step so a trust-v1 bootstrap can
#      construct the approved seed/config first and that config can initialize
#      the one real runtime directly from genesis.
#
# It is ADDITIVE — a separate script from setup_repo.sh — so the all-math /
# revision setup paths are byte-unaffected (the new script never runs for them).
#
# Usage:
#   ./scripts/setup_pv_repo.sh \
#     --crate <rust-crate-dir> \
#     [--unadapted-crate <adapter-input-crate-dir>] \
#     --out-repo <new-working-repo> \
#     [--runtime-root <runtime-root>] \
#     [--node-prefix <prefix>] \
#     [--project-slug <slug>] \
#     [--charon-bin <path>] [--aeneas-bin <path>] \
#     [--opam-switch 5.3.0] [--lean-toolchain leanprover/lean4:v4.30.0-rc2] \
#     [--aeneas-lean-lib <path-to-aeneas/backends/lean>] \
#     [--cargo-target-dir <dir>] [--work-root <dir>] \
#     [--goal <path>] \
#     [--config-template <trellis.config.json>] \
#     [--phase0-genesis <canonical-genesis.json>] \
#     [--sealed-generation <canonical-sealed-generation.json>] \
#     [--filespec-checker <prebuilt-scan_tablet_filespec>] \
#     [--prescribed-region-checker <prebuilt-print_prescribed_region>] \
#     [--extraction-config <json-file>] \
#     [--prepare-only] [--prewarm-library-only] \
#     [--skeleton-only] \
#     [--defer-runtime-init] \
#     [--prewarm-from <compatible-repo>] \
#     [--gate]
#
# --skeleton-only seeds the EXTRACTION SCAFFOLD ONLY: the Preamble + one model
# node per extracted decl + their byte-pinned challenge targets + config + runtime
# state, with NO goal nodes. The verification GOALS are deferred — supply them
# later via a follow-up generic prose seed. In this mode --goal is rejected because
# a skeleton has no goals. configured_targets stays empty and no goal Tablet node
# is written.
#
# --goal (REQUIRED outside --skeleton-only) is the authored prose goal referent. Its exact bytes
# are UTF-8 validated and copied to repo-root GOAL.md. Goal functions are derived
# only from visible `### \`function\`` headings; malformed code headings, duplicate
# names, prefix-ambiguous names, and a heading-less goal fail before extraction.
# The intermediate config records names without claiming a resolution. Trust
# bootstrap has the launch-contract registry and rewrites each entry to `decide`
# exactly when its evaluation contract is executable-deferred; routing-only
# entries become `prove`.
#
# --gate runs the Slice-2 acceptance gates (node_role / challenge-target /
# baseline seeded; the byte-pin rejects a Model edit; an empty source_sha256 is
# rejected; all-math setup_repo.sh output unaffected — proven additively).
# It is incompatible with --defer-runtime-init because those gates inspect the
# initialized runtime. Run them after `scripts/trellis.sh init` with the final
# trust-v1 config instead.

set -euo pipefail
umask 0002

usage() {
  sed -n '2,96p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

CRATE=""
UNADAPTED_CRATE=""
OUT_REPO=""
RUNTIME_ROOT=""
NODE_PREFIX=""
PROJECT_SLUG=""
PREWARM_FROM=""
CHARON_BIN="${TRELLIS_ROOT:-/path/to/trellis}/pv_spike/tools/charon/bin/charon"
AENEAS_BIN="${TRELLIS_ROOT:-/path/to/trellis}/pv_spike/tools/aeneas/bin/aeneas"
CHARON_VERSION="0.1.216"
AENEAS_VERSION="fa699427"
CHARON_GIT_URL="https://github.com/AeneasVerif/charon"
OPAM_SWITCH="5.3.0"
LEAN_TOOLCHAIN="leanprover/lean4:v4.30.0-rc2"
AENEAS_LEAN_LIB="${TRELLIS_ROOT:-/path/to/trellis}/pv_spike/tools/aeneas/backends/lean"
# Aeneas `require` form in the generated tablet lakefile. "path" (default,
# used by local and fixture setups) writes a local-absolute-path
# require (`require aeneas from "<aeneas-lean-lib>"`); Aeneas's deps are resolved
# from the local checkout at setup-time prewarm, with NO clone at run time. "git"
# pins the exact commit of the --aeneas-lean-lib checkout as a subdir git-require
# (externally reproducible — a fresh clone of the tablet resolves Aeneas, and via
# Aeneas's own lakefile mathlib @ its pinned rc2 tag, with no local paths); the
# setup-time prewarm still pre-clones it un-sandboxed so the first
# `prepare_compiled_support` finds it cached (a run-time clone into the read-only
# supervisor `.lake/packages` would otherwise EROFS).
AENEAS_REQUIRE_MODE="path"
AENEAS_GIT_URL="https://github.com/AeneasVerif/aeneas"
CARGO_TARGET_DIR_ARG=""
WORK_ROOT_ARG=""
RUN_GATE=0
# Trust-v1 production order: extraction/repo first, then trust bootstrap, then
# exactly one runtime initialization against the trust-bearing config. This
# option deliberately leaves RUNTIME_ROOT absent.
DEFER_RUNTIME_INIT=0
# Path to the authored UTF-8 prose goal. Setup copies its exact bytes to
# GOAL.md and derives configured target names from its rendered target headings.
GOAL_ARG=""
PROSE_GOAL_PARSE_JSON='{"targets":[],"unquoted_headings":[]}'
PHASE0_GENESIS=""
SEALED_GENERATION=""
FILESPEC_CHECKER=""
PRESCRIBED_REGION_CHECKER=""
CONFIG_TEMPLATE_ARG=""
PREPARE_ONLY=0
PREWARM_LIBRARY_ONLY=0
GOAL_MODE="prose"
MODEL_BOUNDARY_CONTRACT_ARG=""
EXTRACTION_CONFIG_ARG=""
# Skeleton-only: seed the extraction scaffold (Preamble + model nodes + their
# byte-pinned challenge targets + config + state) with NO goal/spec nodes. The
# bespoke verification GOALS are deferred. Off by default (the full seed needs a
# goal contract); opt-in keeps every existing path byte-unchanged.
SKELETON_ONLY=0

# PV under-model assumptions (Slice 2): the Rust-docs corpus. Fetched at setup
# (network available then) into $OUT_REPO/rust_docs_corpus/ — NOT shipped with
# trellis (licensing) — and version-pinned per seed. Read in-sandbox by the
# worker (author C), the assumptions lane (resolve+read the locator), and the
# auditor. Skippable offline via --skip-corpus (the assumptions lane then
# rejects every candidate for a non-resolving citation, fail-closed).
SKIP_CORPUS=0
# Pin the exact upstream commit per seed (reproducibility). The defaults below
# are PLACEHOLDERS — override with the real pinned revs at seed time via the
# RUST_REFERENCE_REV / NOMICON_REV / RUST_STD_REV env vars (or edit here). The
# fetch fails loud on an unknown rev so a stale placeholder never silently
# produces a wrong corpus.
RUST_REFERENCE_GIT_URL="${RUST_REFERENCE_GIT_URL:-https://github.com/rust-lang/reference}"
RUST_REFERENCE_REV="${RUST_REFERENCE_REV:-PIN_ME_rust_reference_rev}"
NOMICON_GIT_URL="${NOMICON_GIT_URL:-https://github.com/rust-lang/nomicon}"
NOMICON_REV="${NOMICON_REV:-PIN_ME_nomicon_rev}"
RUST_STD_GIT_URL="${RUST_STD_GIT_URL:-https://github.com/rust-lang/rust}"
RUST_STD_REV="${RUST_STD_REV:-PIN_ME_rust_std_rev}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --crate)            CRATE="$2"; shift 2 ;;
    --unadapted-crate)  UNADAPTED_CRATE="$2"; shift 2 ;;
    --out-repo)         OUT_REPO="$2"; shift 2 ;;
    --runtime-root)     RUNTIME_ROOT="$2"; shift 2 ;;
    --node-prefix)      NODE_PREFIX="$2"; shift 2 ;;
    --project-slug)     PROJECT_SLUG="$2"; shift 2 ;;
    --charon-bin)       CHARON_BIN="$2"; shift 2 ;;
    --aeneas-bin)       AENEAS_BIN="$2"; shift 2 ;;
    --opam-switch)      OPAM_SWITCH="$2"; shift 2 ;;
    --lean-toolchain)   LEAN_TOOLCHAIN="$2"; shift 2 ;;
    --aeneas-lean-lib)  AENEAS_LEAN_LIB="$2"; shift 2 ;;
    --aeneas-require-mode) AENEAS_REQUIRE_MODE="$2"; shift 2 ;;
    --aeneas-git-url)   AENEAS_GIT_URL="$2"; shift 2 ;;
    --cargo-target-dir) CARGO_TARGET_DIR_ARG="$2"; shift 2 ;;
    --work-root)        WORK_ROOT_ARG="$2"; shift 2 ;;
    --goal)             GOAL_ARG="$2"; shift 2 ;;
    --phase0-genesis)   PHASE0_GENESIS="$2"; shift 2 ;;
    --sealed-generation) SEALED_GENERATION="$2"; shift 2 ;;
    --filespec-checker) FILESPEC_CHECKER="$2"; shift 2 ;;
    --prescribed-region-checker) PRESCRIBED_REGION_CHECKER="$2"; shift 2 ;;
    --config-template) CONFIG_TEMPLATE_ARG="$2"; shift 2 ;;
    --prepare-only)     PREPARE_ONLY=1; shift ;;
    --prewarm-library-only) PREWARM_LIBRARY_ONLY=1; shift ;;
    --goal-mode)        GOAL_MODE="$2"; shift 2 ;;
    --model-boundary-contract) MODEL_BOUNDARY_CONTRACT_ARG="$2"; shift 2 ;;
    --extraction-config) EXTRACTION_CONFIG_ARG="$2"; shift 2 ;;
    --skeleton-only)    SKELETON_ONLY=1; shift ;;
    --defer-runtime-init) DEFER_RUNTIME_INIT=1; shift ;;
    --prewarm-from) PREWARM_FROM="$2"; shift 2 ;;
    --skip-corpus)      SKIP_CORPUS=1; shift ;;
    --gate)             RUN_GATE=1; shift ;;
    -h|--help)          usage; exit 0 ;;
    *) echo "ERROR: Unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
done

if [[ "$GOAL_MODE" != "prose" ]]; then
  echo "ERROR: setup_pv_repo.sh accepts prose goals; use --goal for their source." >&2
  exit 1
fi

if [[ "$DEFER_RUNTIME_INIT" -eq 1 && "$RUN_GATE" -eq 1 ]]; then
  echo "ERROR: --gate requires initialized runtime state and is incompatible with --defer-runtime-init." >&2
  exit 1
fi
if [[ "$PREWARM_LIBRARY_ONLY" -eq 1 && ( "$PREPARE_ONLY" -ne 1 || "$SKELETON_ONLY" -ne 1 ) ]]; then
  echo "ERROR: --prewarm-library-only requires --prepare-only --skeleton-only." >&2
  exit 1
fi

if [[ "$DEFER_RUNTIME_INIT" -eq 1 ]]; then
  if [[ -z "$PHASE0_GENESIS" || ! -f "$PHASE0_GENESIS" \
      || -z "$SEALED_GENERATION" || ! -f "$SEALED_GENERATION" \
      || -z "$FILESPEC_CHECKER" || ! -x "$FILESPEC_CHECKER" \
      || -z "$PRESCRIBED_REGION_CHECKER" || ! -x "$PRESCRIBED_REGION_CHECKER" ]]; then
    echo "ERROR: --defer-runtime-init requires sealed inputs and both prebuilt kernel gate checkers." >&2
    exit 1
  fi
elif [[ -n "$PHASE0_GENESIS" || -n "$SEALED_GENERATION" \
    || -n "$FILESPEC_CHECKER" || -n "$PRESCRIBED_REGION_CHECKER" ]]; then
  echo "ERROR: Phase-0 sealed inputs and gate checkers are accepted only with --defer-runtime-init." >&2
  exit 1
fi

# Deferred initialization is the production trust-v1 construction path.  Its
# sole advance gate must not approve a mutable absolute-path dependency.
# Legacy and fixture setups may still select path mode explicitly, but a
# trust-bearing run must use the exact clean Aeneas git revision checked below.
if [[ "$DEFER_RUNTIME_INIT" -eq 1 && "$AENEAS_REQUIRE_MODE" != "git" ]]; then
  echo "ERROR: --defer-runtime-init requires --aeneas-require-mode git; trust-v1 cannot approve a mutable path dependency." >&2
  exit 1
fi

if [[ "$SKELETON_ONLY" -eq 1 ]]; then
  if [[ -n "$GOAL_ARG" ]]; then
    echo "ERROR: --goal is incompatible with --skeleton-only (a skeleton seed has no goals)." >&2
    exit 1
  fi
elif [[ -z "$GOAL_ARG" || ! -f "$GOAL_ARG" ]]; then
  echo "ERROR: --goal <path> is required (the authored UTF-8 GOAL.md referent)." >&2
  exit 1
fi

if [[ -z "$CRATE" || -z "$OUT_REPO" ]]; then
  echo "ERROR: --crate and --out-repo are required." >&2
  usage >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

resolve() { python3 -c 'import sys,pathlib; print(pathlib.Path(sys.argv[1]).resolve())' "$1"; }

CRATE="$(resolve "$CRATE")"
if [[ -z "$UNADAPTED_CRATE" ]]; then
  UNADAPTED_CRATE="$CRATE"
else
  UNADAPTED_CRATE="$(resolve "$UNADAPTED_CRATE")"
fi
OUT_REPO="$(resolve "$OUT_REPO")"
CHARON_BIN="$(resolve "$CHARON_BIN")"
AENEAS_BIN="$(resolve "$AENEAS_BIN")"
AENEAS_LEAN_LIB="$(resolve "$AENEAS_LEAN_LIB")"
if [[ -n "$GOAL_ARG" ]]; then GOAL_ARG="$(resolve "$GOAL_ARG")"; fi
if [[ -n "$PHASE0_GENESIS" ]]; then PHASE0_GENESIS="$(resolve "$PHASE0_GENESIS")"; fi
if [[ -n "$SEALED_GENERATION" ]]; then SEALED_GENERATION="$(resolve "$SEALED_GENERATION")"; fi
if [[ -n "$FILESPEC_CHECKER" ]]; then FILESPEC_CHECKER="$(resolve "$FILESPEC_CHECKER")"; fi
if [[ -n "$PRESCRIBED_REGION_CHECKER" ]]; then PRESCRIBED_REGION_CHECKER="$(resolve "$PRESCRIBED_REGION_CHECKER")"; fi
if [[ -n "$CONFIG_TEMPLATE_ARG" ]]; then
  CONFIG_TEMPLATE_ARG="$(resolve "$CONFIG_TEMPLATE_ARG")"
  [[ -f "$CONFIG_TEMPLATE_ARG" ]] || {
    echo "ERROR: --config-template is not a readable file: $CONFIG_TEMPLATE_ARG" >&2; exit 1; }
fi
DEFAULT_SLUG="$(basename "$OUT_REPO" | sed -E 's/_tablets?$//')"
PROJECT_SLUG="${PROJECT_SLUG:-$DEFAULT_SLUG}"
if [[ -z "$RUNTIME_ROOT" ]]; then RUNTIME_ROOT="${OUT_REPO}-runtime"; else RUNTIME_ROOT="$(resolve "$RUNTIME_ROOT")"; fi
CARGO_TARGET_DIR_RESOLVED="${CARGO_TARGET_DIR_ARG:-$SOURCE_ROOT/.petarget}"
CARGO_TARGET_DIR_RESOLVED="$(resolve "$CARGO_TARGET_DIR_RESOLVED")"
WORK_ROOT="${WORK_ROOT_ARG:-$SOURCE_ROOT/.work/setup_pv}"
WORK_ROOT="$(resolve "$WORK_ROOT")"
if [[ "$DEFER_RUNTIME_INIT" -eq 1 ]]; then
  # The sealed production role has one writable bind: WORK_ROOT. Keep every
  # extraction/build output beneath it even if the outer create path supplied
  # a conventional build-cache location.
  CARGO_TARGET_DIR_RESOLVED="$WORK_ROOT/cargo-target"
fi

if [[ ! -d "$CRATE" || ! -f "$CRATE/Cargo.toml" ]]; then
  echo "ERROR: --crate is not a Rust crate dir (no Cargo.toml): $CRATE" >&2
  exit 1
fi
if [[ ! -d "$UNADAPTED_CRATE" ]]; then
  echo "ERROR: --unadapted-crate is not a source-tree directory: $UNADAPTED_CRATE" >&2
  exit 1
fi

# Parse the authored referent before extraction does any expensive work. The
# parser follows the CommonMark constructs that affect this deliberately narrow
# surface: 0-3-space ATX indentation, fenced-code closing rules, optional ATX
# closing hashes, and HTML comments. Target-looking constructs outside the
# supported top-level form fail loudly. Prefix-related targets are rejected
# because the existing prose auto-claim rule is prefix based and would otherwise
# cross-claim them.
if [[ "$SKELETON_ONLY" -ne 1 ]]; then
  PROSE_GOAL_PARSE_JSON="$(python3 "$SCRIPT_DIR/pv_prose_goal.py" "$GOAL_ARG")"
fi
if [[ ! -x "$CHARON_BIN" || ! -x "$AENEAS_BIN" ]]; then
  echo "ERROR: Charon and Aeneas extraction tools must be executable regular files." >&2
  exit 1
fi

mkdir -p "$WORK_ROOT" "$CARGO_TARGET_DIR_RESOLVED"

if [[ "$DEFER_RUNTIME_INIT" -eq 1 ]]; then
  # Keep the sealed handoff outside the writable sandbox home so its explicit
  # read-only bind cannot be shadowed by the later writable HOME bind.
  OPAM_ENV_JSON="${WORK_ROOT}.opam-switch-environment.json"
  [[ ! -e "$OPAM_ENV_JSON" ]] || {
    echo "ERROR: stale sealed opam environment exists: $OPAM_ENV_JSON" >&2; exit 1; }
  while IFS=$'\t' read -r _opam_name _opam_value; do
    case "$_opam_name" in
      OPAM_SWITCH_PREFIX)
        export "$_opam_name=$_opam_value"
        _opam_switch_prefix="$_opam_value"
        ;;
      OPAMSWITCH|CAML_LD_LIBRARY_PATH|OCAML_TOPLEVEL_PATH|OCAMLTOP_INCLUDE_PATH)
        export "$_opam_name=$_opam_value"
        ;;
      *) echo "ERROR: Genesis produced an unapproved opam variable." >&2; exit 1 ;;
    esac
  done < <(python3 - "$PHASE0_GENESIS" "$OPAM_ENV_JSON" "$OPAM_SWITCH" "$SOURCE_ROOT" \
    "$FILESPEC_CHECKER" "$PRESCRIBED_REGION_CHECKER" <<'PY'
import hashlib, json, os, stat, sys
from pathlib import Path

genesis_path = Path(sys.argv[1])
output_path = Path(sys.argv[2])
expected_switch = sys.argv[3]
sys.path.insert(0, str(Path(sys.argv[4])))
from scripts.pv_source_tree import readonly_closure_digest
raw = genesis_path.read_bytes()
genesis = json.loads(raw)
canonical = lambda value: json.dumps(
    value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
).encode("utf-8")
if canonical(genesis) != raw:
    raise SystemExit("Phase-0 Genesis is not canonical JSON")
toolchain = genesis.get("toolchain")
components = genesis.get("components")
if not isinstance(toolchain, dict) or toolchain.get("opam_switch") != expected_switch:
    raise SystemExit("Phase-0 Genesis selected a different opam switch")
for field, supplied in (
    ("filespec_checker", Path(sys.argv[5])),
    ("prescribed_region_checker", Path(sys.argv[6])),
):
    pin = components.get(field) if isinstance(components, dict) else None
    mode = supplied.lstat().st_mode
    resolved = supplied.resolve(strict=True)
    if (
        supplied.is_symlink()
        or not stat.S_ISREG(mode)
        or not os.access(resolved, os.X_OK)
        or not isinstance(pin, dict)
        or set(pin) != {"path", "sha256"}
        or pin["path"] != str(resolved)
        or pin["sha256"] != hashlib.sha256(resolved.read_bytes()).hexdigest()
    ):
        raise SystemExit(f"Phase-0 Genesis {field} pin differs from the supplied executable")
prefix_pin = toolchain.get("opam_switch_prefix")
environment = toolchain.get("opam_switch_environment")
keys = {
    "OPAM_SWITCH_PREFIX", "OPAMSWITCH", "CAML_LD_LIBRARY_PATH",
    "OCAML_TOPLEVEL_PATH", "OCAMLTOP_INCLUDE_PATH",
}
if not isinstance(prefix_pin, dict) or set(prefix_pin) != {"path", "source_tree_sha256"}:
    raise SystemExit("Phase-0 Genesis has a malformed opam switch prefix pin")
if not isinstance(environment, dict) or set(environment) != keys or not all(
    isinstance(key, str) and isinstance(value, str) and not any(
        character in key + value for character in ("\0", "\n", "\r", "\t")
    )
    for key, value in environment.items()
):
    raise SystemExit("Phase-0 Genesis has a malformed opam switch environment")
prefix_raw = prefix_pin["path"]
prefix = Path(prefix_raw).resolve(strict=True)
if prefix_raw != str(prefix) or environment["OPAM_SWITCH_PREFIX"] != prefix_raw:
    raise SystemExit("Phase-0 Genesis opam switch prefix is not normalized and consistent")
if environment["OPAMSWITCH"] != expected_switch or not (prefix / "bin").is_dir():
    raise SystemExit("Phase-0 Genesis opam switch identity is inconsistent")
if readonly_closure_digest(prefix) != prefix_pin["source_tree_sha256"]:
    raise SystemExit("Phase-0 Genesis opam switch prefix closure drifted")
for key in keys - {"OPAMSWITCH"}:
    paths = environment[key].split(os.pathsep)
    if not paths or any(not item for item in paths):
        raise SystemExit(f"Phase-0 Genesis opam variable {key} has an empty path")
    for item in paths:
        if not Path(item).resolve(strict=True).is_relative_to(prefix):
            raise SystemExit(f"Phase-0 Genesis opam variable {key} escapes its prefix")
output_path.write_bytes(canonical(environment))
for key in sorted(environment):
    print(f"{key}\t{environment[key]}")
PY
  )
  [[ -n "${_opam_switch_prefix:-}" && -d "$_opam_switch_prefix" ]] || {
    echo "ERROR: Phase-0 Genesis did not yield an available opam switch prefix." >&2; exit 1; }
  _validate_extraction_tool_checkout() {
    local tool_name="$1" executable="$2" expected_origin="$3" expected_commit_prefix="$4"
    local root origin commit
    root="$(git -C "$(dirname "$executable")" rev-parse --show-toplevel 2>/dev/null)" || {
      echo "ERROR: $tool_name executable is not inside a git checkout: $executable" >&2; return 1; }
    case "$executable" in
      "$root"/*) ;;
      *) echo "ERROR: $tool_name executable lies outside its resolved git checkout." >&2; return 1 ;;
    esac
    [[ -z "$(git -C "$root" status --porcelain --untracked-files=all)" ]] || {
      echo "ERROR: $tool_name checkout is dirty; trust-v1 extraction requires exact clean tool sources." >&2
      return 1
    }
    origin="$(git -C "$root" remote get-url origin 2>/dev/null)" || {
      echo "ERROR: $tool_name checkout has no readable origin." >&2; return 1; }
    [[ "${origin%.git}" == "${expected_origin%.git}" ]] || {
      echo "ERROR: $tool_name origin $origin differs from $expected_origin." >&2; return 1; }
    commit="$(git -C "$root" rev-parse HEAD 2>/dev/null)" || return 1
    if [[ -n "$expected_commit_prefix" && "$commit" != "$expected_commit_prefix"* ]]; then
      echo "ERROR: $tool_name commit $commit differs from pinned prefix $expected_commit_prefix." >&2
      return 1
    fi
  }
  _validate_extraction_tool_checkout \
    Charon "$CHARON_BIN" "$CHARON_GIT_URL" ""
  _validate_extraction_tool_checkout \
    Aeneas "$AENEAS_BIN" "$AENEAS_GIT_URL" "$AENEAS_VERSION"
  _charon_root="$(git -C "$(dirname "$CHARON_BIN")" rev-parse --show-toplevel)"
  _aeneas_root="$(git -C "$(dirname "$AENEAS_BIN")" rev-parse --show-toplevel)"
  _charon_version="$(python3 - "$_charon_root/charon/Cargo.toml" <<'PY'
import re, sys
text = open(sys.argv[1], encoding="utf-8").read()
match = re.search(r'^version\s*=\s*"([^"]+)"', text, flags=re.MULTILINE)
if match is None:
    raise SystemExit("missing Charon package version")
print(match.group(1))
PY
)"
  [[ "$_charon_version" == "$CHARON_VERSION" ]] || {
    echo "ERROR: Charon package version $_charon_version differs from $CHARON_VERSION." >&2; exit 1; }
  _aeneas_embedded_version="$("$AENEAS_BIN" -version 2>&1)"
  [[ "$_aeneas_embedded_version" == *"$AENEAS_VERSION"* ]] || {
    echo "ERROR: Aeneas executable version does not report pinned commit $AENEAS_VERSION." >&2; exit 1; }
  _charon_toolchain_root="$("$CHARON_BIN" toolchain-path)"
  [[ -x "$_charon_toolchain_root/bin/rustc" ]] || {
    echo "ERROR: Charon's selected toolchain has no rustc: $_charon_toolchain_root" >&2; exit 1; }
fi
# v1: never overwrite.
if [[ -e "$OUT_REPO" ]]; then
  echo "ERROR: --out-repo already exists: $OUT_REPO (no overwrite in v1)" >&2
  exit 1
fi
if [[ -e "$RUNTIME_ROOT" ]]; then
  echo "ERROR: --runtime-root already exists: $RUNTIME_ROOT (no overwrite in v1)" >&2
  exit 1
fi

echo "Preparing PV tablet repo:"
echo "  Crate:        $CRATE"
echo "  Out repo:     $OUT_REPO"
echo "  Runtime root: $RUNTIME_ROOT"
echo "  Node prefix:  ${NODE_PREFIX:-<none>}"
if [[ "$SKELETON_ONLY" -eq 1 ]]; then
  echo "  Skeleton-only: model nodes only (goals deferred)"
else
  echo "  Goal referent: $GOAL_ARG"
  echo "  Parsed GOAL.md: $PROSE_GOAL_PARSE_JSON"
fi
echo "  Lean tc:      $LEAN_TOOLCHAIN"
echo "  Aeneas lib:   $AENEAS_LEAN_LIB"

# ── 1. Run Slice 1: extract + FILESPEC-split into the repo's Tablet/. ────────
EXTRACTION_GATE_ARGS=()
EXTRACTION_OPAM_ARGS=()
MODEL_BOUNDARY_ARGS=()
EXTRACTION_CONFIG_ARGS=()
if [[ -n "$EXTRACTION_CONFIG_ARG" ]]; then
  EXTRACTION_CONFIG_ARGS=(--extraction-config "$EXTRACTION_CONFIG_ARG")
fi
if [[ -n "$MODEL_BOUNDARY_CONTRACT_ARG" ]]; then
  MODEL_BOUNDARY_ARGS=(
    --model-boundary-contract "$MODEL_BOUNDARY_CONTRACT_ARG"
  )
fi
if [[ "$DEFER_RUNTIME_INIT" -eq 1 ]]; then
  # Trust-v1 bootstrap approves extraction bytes only after a second clean
  # extraction is byte-identical and the real kernel re-derives every FILESPEC
  # region.  These are extraction gates and do not inspect runtime state; the
  # later setup --gate remains deferred until the one final initialization.
  EXTRACTION_GATE_ARGS=(
    --gate
    --kernel-manifest "$SOURCE_ROOT/kernel/Cargo.toml"
    --filespec-checker "$FILESPEC_CHECKER"
    --prescribed-region-checker "$PRESCRIBED_REGION_CHECKER"
  )
  EXTRACTION_OPAM_ARGS=(--opam-env-json "$OPAM_ENV_JSON")
fi
EXTRACTION_OUT="$OUT_REPO"
if [[ "$DEFER_RUNTIME_INIT" -eq 1 ]]; then
  # Production extraction has the same closed, networkless role as every
  # Phase-0 checker execution. Only its scratch home is writable; source,
  # scripts, exact tool checkouts, Charon's selected sysroot, and the sealed
  # opam switch prefix/environment are read-only. The completed tree crosses
  # the boundary with one atomic rename.
  EXTRACTION_OUT="$WORK_ROOT/phase0-production-extracted"
  [[ ! -e "$EXTRACTION_OUT" ]] || {
    echo "ERROR: stale production extraction tree exists: $EXTRACTION_OUT" >&2; exit 1; }
  mkdir -p "$WORK_ROOT/cargo-home" "$WORK_ROOT/tmp"
  export PATH="$_charon_toolchain_root/bin:$(dirname "$CHARON_BIN"):$(dirname "$AENEAS_BIN"):$_opam_switch_prefix/bin:/usr/bin:/bin"
  export CARGO_HOME="$WORK_ROOT/cargo-home"
  export TMPDIR="$WORK_ROOT/tmp" TMP="$WORK_ROOT/tmp" TEMP="$WORK_ROOT/tmp"
  export LANG="C.UTF-8" LC_ALL="C.UTF-8"
  export CHARON_TOOLCHAIN_IS_IN_PATH=1
  export LD_LIBRARY_PATH="$_charon_toolchain_root/lib"
  export CARGO_NET_OFFLINE=true
  unset OPAMROOT RUSTC RUSTDOC RUSTUP_TOOLCHAIN
fi
EXTRACTION_COMMAND=(python3 "$SCRIPT_DIR/extract_pv_model.py" \
  --crate "$CRATE" \
  --out "$EXTRACTION_OUT" \
  --node-prefix "$NODE_PREFIX" \
  --charon-bin "$CHARON_BIN" \
  --aeneas-bin "$AENEAS_BIN" \
  --opam-switch "$OPAM_SWITCH" \
  "${EXTRACTION_OPAM_ARGS[@]}" \
  --charon-version "$CHARON_VERSION" \
  --aeneas-version "$AENEAS_VERSION" \
  --cargo-target-dir "$CARGO_TARGET_DIR_RESOLVED" \
  --work-root "$WORK_ROOT" \
  "${MODEL_BOUNDARY_ARGS[@]}" \
  "${EXTRACTION_CONFIG_ARGS[@]}" \
  "${EXTRACTION_GATE_ARGS[@]}")
if [[ "$DEFER_RUNTIME_INIT" -eq 1 ]]; then
  EXTRACT_SUMMARY="$(python3 "$SCRIPT_DIR/pv_source_adaptation_sandbox.py" \
    --work-dir "$SOURCE_ROOT" \
    --burst-home "$WORK_ROOT" \
    --readonly "$CRATE" \
    --readonly "$_charon_root" \
    --readonly "$_aeneas_root" \
    --readonly "$_charon_toolchain_root" \
    --readonly "$_opam_switch_prefix" \
    --readonly "$OPAM_ENV_JSON" \
    --readonly "$FILESPEC_CHECKER" \
    --readonly "$PRESCRIBED_REGION_CHECKER" \
    -- "${EXTRACTION_COMMAND[@]}")"
  mv -- "$EXTRACTION_OUT" "$OUT_REPO"
else
  EXTRACT_SUMMARY="$("${EXTRACTION_COMMAND[@]}")"
fi
echo "  Extracted model into $OUT_REPO/Tablet (summary: $EXTRACT_SUMMARY)"

PV_TABLET_JSON="$OUT_REPO/pv_tablet.json"
if [[ ! -f "$PV_TABLET_JSON" ]]; then
  echo "ERROR: extract_pv_model.py did not produce $PV_TABLET_JSON" >&2
  exit 1
fi

# Bind heading-derived names through the same canonical reporter Phase 0 uses.
if [[ "$SKELETON_ONLY" -ne 1 ]]; then
  GOAL_BINDING_REPORT="$WORK_ROOT/goal-binding-report.json"
  GOAL_BINDING_UNADAPTED_TREE="$CRATE"
  if [[ "$DEFER_RUNTIME_INIT" -eq 1 ]]; then
    GOAL_BINDING_UNADAPTED_TREE="$UNADAPTED_CRATE"
  fi
  GOAL_BINDING_ARGS=(
    --goal "$GOAL_ARG"
    --pv-tablet "$PV_TABLET_JSON"
    --adapted-tree "$CRATE"
    --unadapted-tree "$GOAL_BINDING_UNADAPTED_TREE"
    --out "$GOAL_BINDING_REPORT"
    --require-complete
    # The model this extraction just produced is what the report reads for
    # declarations left without a definition; the sealed report read the
    # byte-identical model the final check produced.
    --model-root "$OUT_REPO"
  )
  if [[ "$DEFER_RUNTIME_INIT" -eq 1 ]]; then
    GOAL_BINDING_LEDGER="$WORK_ROOT/adaptation-ledger.json"
    GOAL_BINDING_PARTITIONS="$WORK_ROOT/source-partition-manifest.json"
    python3 - "$SEALED_GENERATION" "$GOAL_BINDING_LEDGER" "$GOAL_BINDING_PARTITIONS" <<'PY'
import json, sys
from pathlib import Path
sealed = json.loads(Path(sys.argv[1]).read_bytes())
Path(sys.argv[2]).write_text(
    json.dumps(sealed["semantic_bundle"]["ledger"], sort_keys=True, separators=(",", ":")),
    encoding="utf-8",
)
Path(sys.argv[3]).write_text(
    json.dumps(sealed["semantic_bundle"]["source_partition_manifest"], sort_keys=True, separators=(",", ":")),
    encoding="utf-8",
)
PY
    GOAL_BINDING_ARGS+=(
      --ledger "$GOAL_BINDING_LEDGER"
      --partition-manifest "$GOAL_BINDING_PARTITIONS"
      --expected-sealed-generation "$SEALED_GENERATION"
    )
  fi
  python3 "$SCRIPT_DIR/pv_goal_binding.py" \
    "${GOAL_BINDING_ARGS[@]}"
fi

# ── 2. Scaffold the repo (mirror setup_repo.sh; PV-pinned lakefile). ─────────
mkdir -p "$OUT_REPO/.trellis/logs" "$OUT_REPO/.trellis/scripts" "$OUT_REPO/.trellis/checkpoints"
mkdir -p "$OUT_REPO/.trellis/staging" "$OUT_REPO/.trellis/viewer/state-at" "$OUT_REPO/.trellis/chats" "$OUT_REPO/.trellis/scratch"
mkdir -p "$OUT_REPO/.trellis/runtime"
if [[ "$DEFER_RUNTIME_INIT" -eq 1 ]]; then
  PHASE0_REPO="$OUT_REPO/.trellis/phase0"
  mkdir "$PHASE0_REPO"
  cp -- "$PHASE0_GENESIS" "$PHASE0_REPO/GENESIS.json"
  cp -- "$SEALED_GENERATION" "$PHASE0_REPO/SEALED_GENERATION.json"
  cp -- "$GOAL_BINDING_REPORT" "$PHASE0_REPO/GOAL_BINDING_REPORT.json"
  python3 - "$SEALED_GENERATION" "$PHASE0_REPO" <<'PY'
import json, sys
from pathlib import Path

sealed = json.loads(Path(sys.argv[1]).read_bytes())
root = Path(sys.argv[2])
canonical = lambda value: json.dumps(
    value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
).encode("utf-8")
(root / "BUNDLE.json").write_bytes(canonical(sealed["semantic_bundle"]))
(root / "ADAPTATION_LEDGER.json").write_bytes(
    canonical(sealed["semantic_bundle"]["ledger"])
)
(root / "SOURCE_PARTITION_MANIFEST.json").write_bytes(
    canonical(sealed["semantic_bundle"]["source_partition_manifest"])
)
PY
  cmp -s -- "$PHASE0_GENESIS" "$PHASE0_REPO/GENESIS.json" || {
    echo "ERROR: copied Phase-0 genesis changed bytes" >&2; exit 1; }
  cmp -s -- "$SEALED_GENERATION" "$PHASE0_REPO/SEALED_GENERATION.json" || {
    echo "ERROR: copied sealed Phase-0 generation changed bytes" >&2; exit 1; }
fi

if [[ "$DEFER_RUNTIME_INIT" -eq 1 ]]; then
# Copy both sealed trees byte-for-byte into the repo. Source-manifest equality
# is checked by the trust kernel before bootstrap, so no path (including a
# worker-authored path) may be silently filtered at this boundary.
mkdir -p "$OUT_REPO/crate" "$OUT_REPO/unadapted-crate"
python3 - "$CRATE" "$OUT_REPO/crate" "$UNADAPTED_CRATE" "$OUT_REPO/unadapted-crate" <<'PY'
import shutil, sys
from pathlib import Path
for source_arg, destination_arg in ((sys.argv[1], sys.argv[2]), (sys.argv[3], sys.argv[4])):
    src = Path(source_arg); dst = Path(destination_arg)
    shutil.copytree(
        src,
        dst,
        dirs_exist_ok=True,
    )
PY
else
  PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$CRATE" "$OUT_REPO" "$UNADAPTED_CRATE" <<'PY_COPY'
import sys
from pathlib import Path
from scripts.extract_pv_model import copy_source_inputs
copy_source_inputs(Path(sys.argv[1]), Path(sys.argv[2]) / "crate")
copy_source_inputs(Path(sys.argv[3]), Path(sys.argv[2]) / "unadapted-crate")
PY_COPY
fi
echo "  Recorded crate sources at $OUT_REPO/crate"
echo "  Recorded adapter-input crate sources at $OUT_REPO/unadapted-crate"

# The authored prose is a seed input, not a generated manifest. It was
# decoded above only for validation/target derivation; copy the original bytes
# verbatim so the runtime and trust bootstrap hash precisely what the operator
# supplied (including line endings and a possible UTF-8 BOM).
if [[ "$SKELETON_ONLY" -ne 1 ]]; then
  cp -- "$GOAL_ARG" "$OUT_REPO/GOAL.md"
  cmp -s -- "$GOAL_ARG" "$OUT_REPO/GOAL.md" || {
    echo "ERROR: copied GOAL.md differs from --goal input bytes" >&2
    exit 1
  }
  echo "  Copied authored prose goal bytes to $OUT_REPO/GOAL.md"
fi

# PV FILESPEC variant: identical to the math FILESPEC except the
# protected-correspondence section is re-titled "approved-target preservation"
# (the math one says "paper-target preservation" / "approves a paper target",
# referencing a paper advance-gate that PV has no analog of — PV's gate approves
# a verification / challenge target). Same splitter semantics.
cp "$SOURCE_ROOT/trellis/prompt_fragments/pv/FILESPEC.md" "$OUT_REPO/FILESPEC.md"
# PV materializes the PV canonical rubrics at root, NOT the all-math ones. The
# every-role prompt names the root SOUNDNESS.md/SUBSTANTIVENESS.md/CORRESPONDENCE.md
# as the authoritative lane definitions; the all-math SOUNDNESS/SUBSTANTIVENESS
# say "the paper" and directly contradict the inlined PV rubrics, so copy the
# pv/canonical/* variants instead. CORRESPONDENCE is domain-neutral (kept from
# the shared canonical). Prose goals use the shared faithfulness standard with
# the PV reference guidance. The byte-pinned model has no deviations lane.
  cp "$SOURCE_ROOT/trellis/prompt_fragments/pv/canonical/SOUNDNESS.md" "$OUT_REPO/SOUNDNESS.md"
  cp "$SOURCE_ROOT/trellis/prompt_fragments/pv/canonical/SUBSTANTIVENESS.md" "$OUT_REPO/SUBSTANTIVENESS.md"
  cat "$SOURCE_ROOT/trellis/prompt_fragments/canonical/FAITHFULNESS.md" \
      "$SOURCE_ROOT/trellis/prompt_fragments/pv/canonical/FAITHFULNESS.md" > "$OUT_REPO/FAITHFULNESS.md"
cp "$SOURCE_ROOT/trellis/prompt_fragments/canonical/CORRESPONDENCE.md" "$OUT_REPO/CORRESPONDENCE.md"
# Process rules are mode-shared (one canonical doc for math and PV; PV
# differences are marked inline), so PV copies the shared canonical file.
cp "$SOURCE_ROOT/trellis/prompt_fragments/canonical/PROCESS_RULES.md" "$OUT_REPO/PROCESS_RULES.md"
echo "  Wrote FILESPEC.md, PV verifier rubrics, and PROCESS_RULES.md"

# PV lakefile: the math lakefile but with the Aeneas `require` ADDED (so the
# generated model's `import Aeneas` resolves) and NO mathlib require (the model
# imports Aeneas, which transitively pulls mathlib via its own lakefile). The
# math lakefile path is untouched (this is a separate file in a separate repo).
# Static part via a QUOTED heredoc so the Lean backtick (⟨`autoImplicit⟩) is not
# command-substituted; the interpolated Aeneas path is appended separately.
cat > "$OUT_REPO/lakefile.lean" <<'LAKEFILE'
import Lake
open Lake DSL

package «tablet» where
  leanOptions := #[
    ⟨`autoImplicit, false⟩
  ]

@[default_target]
lean_lib «Tablet» where
  srcDir := "."

-- PV: the Aeneas support library (transitively requires mathlib @ rc2). The
-- generated ExtractionModel nodes import Aeneas.
LAKEFILE
# Append the Aeneas `require`. Default `path` mode: a local-absolute-path
# require resolved from the
# --aeneas-lean-lib checkout — no run-time clone. `git` mode pins the exact
# commit of that checkout as a subdir git-require, so a fresh clone resolves
# Aeneas (and, via Aeneas's own lakefile, mathlib @ its pinned rc2 tag) with NO
# local paths (externally reproducible); the prewarm above pre-clones it.
if [[ "$AENEAS_REQUIRE_MODE" == "git" ]]; then
  _aeneas_root="$(git -C "$AENEAS_LEAN_LIB" rev-parse --show-toplevel 2>/dev/null)" || {
    echo "ERROR: --aeneas-lean-lib ($AENEAS_LEAN_LIB) is not inside a git checkout; cannot" \
         "derive a git pin. Pass --aeneas-require-mode path for a local-path require." >&2
    exit 1; }
  _aeneas_commit="$(git -C "$AENEAS_LEAN_LIB" rev-parse HEAD 2>/dev/null)"
  [[ "$_aeneas_commit" =~ ^[0-9a-f]{40}$ ]] || {
    echo "ERROR: could not resolve a 40-char Aeneas commit (got '$_aeneas_commit')." >&2; exit 1; }
  [[ -z "$(git -C "$_aeneas_root" status --porcelain --untracked-files=all)" ]] || {
    echo "ERROR: Aeneas checkout is dirty; trust-v1 git mode requires an exact clean source tree." >&2
    exit 1
  }
  _aeneas_origin="$(git -C "$_aeneas_root" remote get-url origin 2>/dev/null)" || {
    echo "ERROR: Aeneas checkout has no readable origin remote; cannot bind the Lake git dependency." >&2
    exit 1
  }
  [[ "${_aeneas_origin%.git}" == "${AENEAS_GIT_URL%.git}" ]] || {
    echo "ERROR: Aeneas checkout origin ($_aeneas_origin) does not match --aeneas-git-url ($AENEAS_GIT_URL)." >&2
    exit 1
  }
  _aeneas_recorded_version="$(python3 - "$PV_TABLET_JSON" <<'PY'
import json, sys
print(json.load(open(sys.argv[1], encoding="utf-8"))["extraction_toolchain"]["aeneas"])
PY
)"
  [[ -n "$_aeneas_recorded_version" && "$_aeneas_commit" == "$_aeneas_recorded_version"* ]] || {
    echo "ERROR: Aeneas checkout $_aeneas_commit does not match extracted version $_aeneas_recorded_version." >&2
    exit 1
  }
  _aeneas_subdir="${AENEAS_LEAN_LIB#"$_aeneas_root"/}"
  [[ -n "$_aeneas_subdir" && "$_aeneas_subdir" != "$AENEAS_LEAN_LIB" && "$_aeneas_subdir" != *".."* ]] || {
    echo "ERROR: Aeneas Lean library must be a normalized subdirectory of its clean git root." >&2
    exit 1
  }
  printf 'require aeneas from git\n  "%s" @ "%s" / "%s"\n' \
    "$AENEAS_GIT_URL" "$_aeneas_commit" "$_aeneas_subdir" >> "$OUT_REPO/lakefile.lean"
  echo "  Wrote PV lakefile.lean (Aeneas require -> git $AENEAS_GIT_URL @ $_aeneas_commit / $_aeneas_subdir)"
elif [[ "$AENEAS_REQUIRE_MODE" == "path" ]]; then
  printf 'require aeneas from\n  "%s"\n' "$AENEAS_LEAN_LIB" >> "$OUT_REPO/lakefile.lean"
  echo "  Wrote PV lakefile.lean (Aeneas require -> local path $AENEAS_LEAN_LIB)"
else
  echo "ERROR: --aeneas-require-mode must be 'git' or 'path' (got '$AENEAS_REQUIRE_MODE')." >&2
  exit 1
fi

echo "$LEAN_TOOLCHAIN" > "$OUT_REPO/lean-toolchain"
echo "  Wrote lean-toolchain ($LEAN_TOOLCHAIN)"

# ── Prewarm: populate $OUT_REPO/.lake/packages un-sandboxed. ─────────────────
# Mirrors the math setup_repo.sh prewarm (trellis.setup_ops.run_setup_prewarm):
# `lake update` resolves + fetches the dep tree (Aeneas + its transitive mathlib
# @ rc2), `lake exe cache get` downloads the prebuilt mathlib oleans, and
# `lake build Tablet.Preamble` builds Aeneas's own support oleans. The latter is
# required because Aeneas has no public Lake cache and package sources are
# read-only once the supervisor sandbox starts. Run with the elan toolchain on
# PATH (lake lives at ~/.elan/bin, not the system PATH) and a fake HOME so it
# does not touch the operator's caches.
#
# WHY this is required on a fresh PV run: the supervisor-workspace bootstrap
# (`trellis.supervisor_workspace._ensure_supervisor_lake_packages`) hardlinks
# <repo>/.lake/packages → the supervisor repo so the first
# `prepare_compiled_support` does NOT resolve deps inside the read-only
# `lake_compiler` sandbox — but it SHORT-CIRCUITS when <repo>/.lake/packages is
# empty. Without this prewarm that dir is empty, the supervisor falls through to
# resolving deps in the RO sandbox, and startup dies with
# `prepare_compiled_support failed … read-only file system` (cloning mathlib, or
# locking the Aeneas lakefile, inside the sandboxed RO `.lake`). In git
# require-mode this also pre-clones Aeneas itself un-sandboxed (the run-time
# clone would EROFS the same way).
PV_ELAN_HOME="${ELAN_HOME:-$HOME/.elan}"
# lake is at $ELAN_HOME/bin; cargo at ~/.cargo/bin (Charon's nightly is only
# needed at extraction time, already done above — but keep it for parity).
PV_PREWARM_PATH="${PV_PREWARM_PATH:-$PV_ELAN_HOME/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin}"
# A fake HOME so the prewarm's git/lake state stays out of the operator's $HOME.
# A deferred trust-v1 setup must leave RUNTIME_ROOT wholly absent until the one
# initialization from the final trust-bearing config, so its prewarm state lives
# under the operator-owned extraction work root instead.
if [[ -z "${PV_PREWARM_HOME:-}" ]]; then
  if [[ "$DEFER_RUNTIME_INIT" -eq 1 ]]; then
    PV_PREWARM_HOME="$WORK_ROOT/prewarm-home"
  else
    PV_PREWARM_HOME="$RUNTIME_ROOT/prewarm-home"
  fi
fi
mkdir -p "$PV_PREWARM_HOME"
PREWARM_FETCH="lake update; lake exe cache get"
if [[ -n "$PREWARM_FROM" ]]; then
  PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$PREWARM_FROM" "$OUT_REPO" <<'PY'
import shutil, sys
from pathlib import Path
from trellis.supervisor_workspace import _ensure_supervisor_lake_packages
source, destination = map(Path, sys.argv[1:])
for name in ("lean-toolchain", "lakefile.lean"):
    if (source / name).read_bytes() != (destination / name).read_bytes():
        raise SystemExit(f"prewarm source has a different {name}")
_ensure_supervisor_lake_packages(source, destination)
shutil.copy2(source / "lake-manifest.json", destination / "lake-manifest.json")
PY
  PREWARM_FETCH=":"
fi
echo "  Prewarming Lean dependencies (lake update + cache get + Aeneas build) in $OUT_REPO/.lake/packages ..."
PV_PREWARM_TARGET="Tablet.Preamble"
if [[ "$PREWARM_LIBRARY_ONLY" -eq 1 ]]; then
  PV_PREWARM_TARGET="Aeneas"
fi
PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 -m trellis.pv_execution -- env \
  HOME="$PV_PREWARM_HOME" \
  ELAN_HOME="$PV_ELAN_HOME" \
  PATH="$PV_PREWARM_PATH" \
  LEAN_PATH="${LEAN_PATH:-}" \
  bash -c "
    set -euo pipefail
    umask 0002
    cd '$OUT_REPO'
    git config --global --add safe.directory '*' >/dev/null 2>&1 || true
    $PREWARM_FETCH
    lake build '$PV_PREWARM_TARGET'
  "
if [[ ! -d "$OUT_REPO/.lake/packages" ]] || ! find "$OUT_REPO/.lake/packages" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
  echo "ERROR: prewarm did not populate $OUT_REPO/.lake/packages (mathlib + Aeneas deps)." >&2
  echo "       The supervisor would die on startup with prepare_compiled_support EROFS." >&2
  exit 1
fi
echo "  Prewarm done: $(find "$OUT_REPO/.lake/packages" -mindepth 1 -maxdepth 1 -type d | wc -l) package(s) in .lake/packages"

# ── PV under-model assumptions (Slice 2): fetch the Rust-docs corpus. ────────
# Corpus = Rust Reference + Rustonomicon + the pinned Rust `library/` source
# tree into an offline-readable repo path, version-pinned per seed. NOT shipped
# with trellis (licensing). Read in-sandbox by the worker
# (author C + cite the locator), the assumptions lane (resolve+read the locator
# — the citation-resolves guard is "the cited section exists in this fetched,
# pinned corpus"), and the auditor (ground the model-deviation ruling). A
# shallow pinned-rev clone keeps it reproducible. --skip-corpus opts out
# (offline / CI): the assumptions lane then fail-closed-rejects every candidate
# (no citation can resolve), so no unreviewed assumption can slip through.
CORPUS_DIR="$OUT_REPO/rust_docs_corpus"
if [[ "$SKIP_CORPUS" -eq 1 ]]; then
  echo "  Skipping Rust-docs corpus fetch (--skip-corpus); the assumptions lane will reject"
  echo "  every candidate for a non-resolving citation until the corpus is provisioned."
  mkdir -p "$CORPUS_DIR"
else
  echo "  Fetching Rust-docs corpus (Rust Reference + Nomicon + Rust library/, pinned) into $CORPUS_DIR ..."
  mkdir -p "$CORPUS_DIR"
  _fetch_pinned_corpus() {
    local name="$1" url="$2" rev="$3" sparse_path="${4:-}" dest="$CORPUS_DIR/$1"
    local resolved_rev
    if [[ -z "$rev" || "$rev" == PIN_ME_* ]]; then
      echo "    ERROR: $name corpus rev is not pinned: $rev" >&2
      return 1
    fi
    # A nested .git can only be residue from an interrupted fetch: completed
    # corpus trees have their repository metadata stripped. It is not evidence
    # that the requested revision or sparse path is present, so always replace
    # it with a fresh checkout.
    rm -rf "$dest"
    git init -q "$dest" || return 1
    git -C "$dest" remote add origin "$url" || return 1
    if [[ -n "$sparse_path" ]]; then
      # rust-lang/rust is large. A partial, cone-mode sparse checkout downloads
      # blobs only for library/ while retaining the exact upstream commit. Do
      # not fall back to cloning the full default-branch history: if the host
      # cannot serve the pin through a partial fetch, fail loudly instead.
      git -C "$dest" sparse-checkout init --cone || return 1
      git -C "$dest" sparse-checkout set "$sparse_path" || return 1
      if git -C "$dest" fetch -q --depth 1 --filter=blob:none origin "$rev" 2>/dev/null; then
        git -C "$dest" checkout -q FETCH_HEAD || return 1
      else
        git -C "$dest" fetch -q --filter=blob:none origin "$rev" || return 1
        git -C "$dest" checkout -q FETCH_HEAD || return 1
      fi
      [[ -d "$dest/$sparse_path" ]] || {
        echo "    ERROR: $name sparse path is absent at the pinned revision: $sparse_path" >&2
        return 1
      }
    elif git -C "$dest" fetch -q --depth 1 origin "$rev" 2>/dev/null; then
      git -C "$dest" checkout -q FETCH_HEAD || return 1
    else
      # Some hosts disallow fetching an arbitrary SHA shallowly; fall back to a
      # full fetch of the default branch, then check out the pinned rev.
      git -C "$dest" fetch -q origin || return 1
      git -C "$dest" checkout -q "$rev" || return 1
    fi
    resolved_rev="$(git -C "$dest" rev-parse --verify HEAD)" || return 1
    [[ "$resolved_rev" =~ ^[0-9a-f]{40}$ ]] || {
      echo "    ERROR: $name fetched revision did not resolve to a commit: $resolved_rev" >&2
      return 1
    }
    CORPUS_FETCH_RESOLVED_REV="$resolved_rev"
    echo "    $name @ $rev ($resolved_rev) -> $dest"
    # The corpus is committed into the run repo as plain, pinned source text.
    # Keep the rev in CORPUS_MANIFEST.json, but do not leave nested git metadata:
    # `git add -A` would otherwise treat these as submodules / embedded repos.
    rm -rf "$dest/.git"
  }
  _fetch_pinned_corpus "rust-reference" "$RUST_REFERENCE_GIT_URL" "$RUST_REFERENCE_REV" \
    || { echo "ERROR: failed to fetch the Rust Reference corpus; rerun with --skip-corpus to defer." >&2; exit 1; }
  RUST_REFERENCE_RESOLVED_REV="$CORPUS_FETCH_RESOLVED_REV"
  _fetch_pinned_corpus "nomicon" "$NOMICON_GIT_URL" "$NOMICON_REV" \
    || { echo "ERROR: failed to fetch the Nomicon corpus; rerun with --skip-corpus to defer." >&2; exit 1; }
  NOMICON_RESOLVED_REV="$CORPUS_FETCH_RESOLVED_REV"
  _fetch_pinned_corpus "rust-std" "$RUST_STD_GIT_URL" "$RUST_STD_REV" "library" \
    || { echo "ERROR: failed to fetch the pinned Rust library/ source corpus; rerun with --skip-corpus to defer." >&2; exit 1; }
  RUST_STD_RESOLVED_REV="$CORPUS_FETCH_RESOLVED_REV"
  cat > "$CORPUS_DIR/CORPUS_MANIFEST.json" <<CORPUSMANIFEST
{
  "schema": "pv-rust-docs-corpus/v1",
  "sources": [
    { "name": "rust-reference", "url": "$RUST_REFERENCE_GIT_URL", "rev": "$RUST_REFERENCE_RESOLVED_REV", "path": "rust-reference/src" },
    { "name": "nomicon",        "url": "$NOMICON_GIT_URL",        "rev": "$NOMICON_RESOLVED_REV",        "path": "nomicon/src" },
    { "name": "rust-std",       "url": "$RUST_STD_GIT_URL",       "rev": "$RUST_STD_RESOLVED_REV",       "path": "rust-std/library" }
  ]
}
CORPUSMANIFEST
  echo "  Rust-docs corpus fetched."
fi

# Complete the extractor-created TCB provenance after the optional docs corpus
# step. The extraction environment itself was captured before Charon ran; this
# late merge only adds the independently fetched documentation revisions (or an
# explicit reason they are unavailable). Keep the shape identical to the Rust
# `record_extraction_provenance` writer used by non-deferred runtime init.
python3 - "$OUT_REPO/tcb_manifest.json" "$CORPUS_DIR/CORPUS_MANIFEST.json" <<'PY'
import json
import sys
from pathlib import Path

manifest_path = Path(sys.argv[1])
corpus_path = Path(sys.argv[2])
manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
provenance = manifest.get("extraction_provenance")
if not isinstance(provenance, dict):
    raise SystemExit(
        f"ERROR: {manifest_path} lacks the extractor-created extraction_provenance object"
    )
not_recorded = provenance.get("not_recorded")
if not isinstance(not_recorded, list) or not all(
    isinstance(entry, dict)
    and isinstance(entry.get("field"), str)
    and entry["field"]
    and isinstance(entry.get("reason"), str)
    and entry["reason"]
    for entry in not_recorded
):
    raise SystemExit(
        f"ERROR: {manifest_path} extraction_provenance.not_recorded is malformed"
    )
not_recorded = [
    entry for entry in not_recorded if entry["field"] != "docs_corpus_revision"
]
if corpus_path.is_file():
    corpus = json.loads(corpus_path.read_text(encoding="utf-8"))
    if not isinstance(corpus.get("schema"), str) or not isinstance(
        corpus.get("sources"), list
    ):
        raise SystemExit(
            f"ERROR: {corpus_path} must carry string schema and array sources"
        )
    provenance["docs_corpus"] = {
        "manifest_path": "rust_docs_corpus/CORPUS_MANIFEST.json",
        "schema": corpus["schema"],
        "sources": corpus["sources"],
    }
else:
    provenance.pop("docs_corpus", None)
    not_recorded.append(
        {
            "field": "docs_corpus_revision",
            "reason": "rust_docs_corpus/CORPUS_MANIFEST.json is absent",
        }
    )
provenance["not_recorded"] = sorted(not_recorded, key=lambda entry: entry["field"])
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
PY
echo "  Updated tcb_manifest.json extraction provenance with docs-corpus status"

# Preamble.tex empty (no paper). Each model node gets an empty .tex (PV nodes
# are not paper-grounded; Faithfulness is structurally absent).
: > "$OUT_REPO/Tablet/Preamble.tex"
python3 - "$OUT_REPO" <<'PY'
from pathlib import Path
import sys
repo = Path(sys.argv[1])
tablet = repo / "Tablet"
for lean in tablet.glob("*.lean"):
    if lean.stem in {"Preamble", "Assumptions"}:
        continue
    tex = tablet / f"{lean.stem}.tex"
    if not tex.exists():
        tex.write_text("")
PY
echo "  Wrote empty Tablet/*.tex (no paper; Faithfulness structurally absent)"

# APPROVED_AXIOMS.json: seed the closure-gate allowlist from the disclosed TCB.
# extract_pv_model.py emits tcb_manifest.json alongside the model. Opaque
# boundary names in the manifest are crate-relative.
# nodes stays {} (crate-wide TCB, not proof-node-specific).
TCB_MANIFEST_JSON="$OUT_REPO/tcb_manifest.json"
if [[ ! -f "$TCB_MANIFEST_JSON" ]]; then
  echo "ERROR: extract_pv_model.py did not produce $TCB_MANIFEST_JSON" >&2
  exit 1
fi
python3 - "$TCB_MANIFEST_JSON" "$OUT_REPO/APPROVED_AXIOMS.json" <<'PY'
import json, sys
from pathlib import Path
manifest = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
namespace = (manifest.get("namespace") or "").strip()
source_names = manifest.get("global")
if not isinstance(source_names, list):
    source_names = [d.get("name") for d in manifest.get("tcb_disclosure", []) if isinstance(d, dict)]

def _qualify(name):
    name = name.strip()
    if namespace and not name.startswith(namespace + "."):
        return f"{namespace}.{name}"
    return name
names = []
seen = set()
for item in source_names:
    if not isinstance(item, str):
        continue
    q = _qualify(item)
    if q and q not in seen:
        names.append(q)
        seen.add(q)
Path(sys.argv[2]).write_text(
    json.dumps({"global": names, "nodes": {}}, indent=2) + "\n", encoding="utf-8"
)
PY
cat > "$OUT_REPO/HUMAN_INPUT.md" <<'HUMAN'
# Human Input

Write human guidance for the supervisor here when requested.
HUMAN
cat > "$OUT_REPO/INPUT_REQUEST.md" <<'REQUEST'
# Input Request

The supervisor will write explicit requests for human input here when needed.
REQUEST
# PV under-model assumptions (Slice 2): seed the assumptions registry and the
# operator-facing review file. The kernel appends a `status:"pending"` entry to
# PROPOSED_ASSUMPTIONS.json when the assumptions lane gates a worker-authored
# Rust under-model assumption; it re-renders ASSUMPTIONS_REVIEW.md and takes the
# end-of-TheoremStating AssumptionReview human gate LIVE while
# any entry is pending. No pending assumptions at seed; the gate stays vacuous.
cat > "$OUT_REPO/PROPOSED_ASSUMPTIONS.json" <<'PROPOSED'
{
  "assumptions": []
}
PROPOSED
cat > "$OUT_REPO/ASSUMPTIONS_REVIEW.md" <<'AREVIEW'
# Assumptions Review

The supervisor rewrites this file when >=1 Rust under-model assumption is pending
human ratification at the end-of-TheoremStating AssumptionReview gate. Empty until
then.
AREVIEW
echo "  Wrote APPROVED_AXIOMS.json + HUMAN_INPUT.md + INPUT_REQUEST.md + PROPOSED_ASSUMPTIONS.json + ASSUMPTIONS_REVIEW.md"

# ── 3. Write trellis.config.json: template + pv_tablet block, no paper. ──────
CONFIG_OUT="$OUT_REPO/trellis.config.json"
POLICY_OUT="$OUT_REPO/trellis.policy.json"
PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - \
    "${CONFIG_TEMPLATE_ARG:-$SOURCE_ROOT/examples/trellis.config.json}" "$SOURCE_ROOT/examples/trellis.policy.json" \
    "$CONFIG_OUT" "$POLICY_OUT" "$OUT_REPO" "$PROJECT_SLUG" "$PV_TABLET_JSON" \
    "$PROSE_GOAL_PARSE_JSON" "$SKELETON_ONLY" <<'PY'
import json, shutil, sys
from pathlib import Path

template = Path(sys.argv[1]); policy_template = Path(sys.argv[2])
config_out = Path(sys.argv[3]); policy_out = Path(sys.argv[4])
repo = Path(sys.argv[5]).resolve(); slug = sys.argv[6]
pv_tablet = json.loads(Path(sys.argv[7]).read_text(encoding="utf-8"))
skeleton_only = sys.argv[9] == "1"
if not skeleton_only:
    # GOAL functions are mechanically derived from rendered headings in the
    # authored, byte-copied GOAL.md. The kernel reads
    # pv_tablet.verification_targets into configured_targets and roots orphan
    # detection on the goal theorems that claim them; the worker AUTHORS them.
    # GOAL.md is the sole resolution authority. The parser defaults an
    # unmarked heading to `decide` and accepts only the exact `[decide]` and
    # `[prove]` markers.
    targets = json.loads(sys.argv[8])["targets"]
    if not isinstance(targets, list) or not targets:
        raise SystemExit("internal error: prose goal target derivation returned no targets")
    pv_tablet["verification_targets"] = [
        {"name": target["name"], "resolution": target["resolution"]}
        for target in targets
    ]

data = json.loads(template.read_text(encoding="utf-8"))
data.pop("_comment", None)
data["repo_path"] = str(repo)
data["state_dir"] = ".trellis"
data["policy_path"] = "trellis.policy.json"

sandbox = data.setdefault("sandbox", {}); sandbox["enabled"] = True; sandbox["backend"] = "bwrap"
data["loogle"] = {"enabled": False}

workflow = data.setdefault("workflow", {})
# PV has NO paper: OMIT paper_tex_path + main_result_*. Faithfulness is
# structurally absent. The `pv_tablet` challenge targets give the run its goal.
workflow.pop("paper_tex_path", None)
workflow.pop("main_result_labels", None)
workflow.pop("main_result_targets", None)
workflow["approved_axioms_path"] = "APPROVED_AXIOMS.json"
workflow["human_input_path"] = "HUMAN_INPUT.md"
workflow["input_request_path"] = "INPUT_REQUEST.md"

# The runtime and trust bootstrap share this top-level referent key/default.
# Set it explicitly for prose seeds so the copied input and its hash-bound
# runtime locator cannot drift with a future template default.
if not skeleton_only:
    data["goal_file"] = "GOAL.md"

tmux = data.setdefault("tmux", {}); tmux["session_name"] = slug
chat = data.setdefault("chat", {})
chat["root_dir"] = ".trellis/chats"; chat["repo_name"] = slug
chat["project_name"] = slug.replace("_", " ").title() + " Verification"

# Splice the pv_tablet block in at top level (NOT under workflow).
data["pv_tablet"] = pv_tablet

config_out.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
# Write the merged target list back for trust-plan derivation.
Path(sys.argv[7]).write_text(
    json.dumps(pv_tablet, indent=2) + "\n", encoding="utf-8"
)
if policy_template.exists():
    shutil.copyfile(policy_template, policy_out)
PY
echo "  Wrote trellis.config.json (pv_tablet block; paper fields omitted)"

# ── 4. Goal nodes are authored by the campaign. ─────────────────────────────
if [[ "$SKELETON_ONLY" -eq 1 ]]; then
  echo "  Skeleton-only: skipped goal-node seeding (model nodes + Preamble only)"
else
  echo "  Generic prose mode: skipped goal-node seeding (worker authors goal/support nodes)"
fi
# ── 5. Init runtime state via the kernel CLI (seeds the pv_tablet block). ────
KERNEL_CLI_BIN=""
if [[ "$DEFER_RUNTIME_INIT" -eq 0 && "$PREPARE_ONLY" -eq 0 ]]; then
  # Build the isolated kernel CLI binary and point run_kernel_cli at it so
  # nothing lands in kernel/target/ (stays in the isolated CARGO_TARGET_DIR).
  if [[ -n "${TRELLIS_TRELLIS_KERNEL_CMD:-}" && -x "$TRELLIS_TRELLIS_KERNEL_CMD" ]]; then
    KERNEL_CLI_BIN="$TRELLIS_TRELLIS_KERNEL_CMD"
  else
    echo "  Building isolated kernel CLI binary..."
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR_RESOLVED" PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" \
      python3 -m trellis.pv_execution -- cargo build -j4 --quiet \
      --manifest-path "$SOURCE_ROOT/kernel/Cargo.toml" --bin trellis_runtime_cli
    KERNEL_CLI_BIN="$CARGO_TARGET_DIR_RESOLVED/debug/trellis_runtime_cli"
  fi
  [[ -x "$KERNEL_CLI_BIN" ]] || { echo "ERROR: kernel CLI binary not built: $KERNEL_CLI_BIN" >&2; exit 1; }
  export TRELLIS_TRELLIS_KERNEL_CMD="$KERNEL_CLI_BIN"

  mkdir -p "$RUNTIME_ROOT"
  PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$RUNTIME_ROOT" "$CONFIG_OUT" <<'PY'
import json, sys
from pathlib import Path
from trellis.runtime.kernel_cli import run_kernel_cli
from trellis.config import load_config
from trellis.checking import write_scripts

runtime_root = Path(sys.argv[1]).resolve()
config_path = Path(sys.argv[2]).resolve()
# InitFromConfig invokes the repo-local checker before the first agent exists.
# Materialize its entry points and matching runtime before invoking the kernel.
config = load_config(config_path)
write_scripts(config.repo_path, config.state_dir)
result = run_kernel_cli({
    "action": "init_from_config",
    "root": str(runtime_root),
    "config_path": str(config_path),
})
if result.get("status") != "ok":
    raise SystemExit(f"init_from_config failed: {json.dumps(result, indent=2)}")
state = result.get("state", {})
roles = state.get("node_role", {})
targets = state.get("configured_challenge_targets", {})
configured_targets = state.get("configured_targets", [])
approved = state.get("approved_extraction_provenance", {})
print("  init_from_config ok:")
print(f"    node_role (ExtractionModel): {sum(1 for v in roles.values() if v=='extraction_model')}")
print(f"    configured_targets ({len(configured_targets)}): " + ", ".join(configured_targets))
print(f"    configured_challenge_targets: {len(targets)}")
print(f"    approved_extraction_provenance: {len(approved)}")
PY
else
  echo "  Deferred runtime initialization for trust-v1 bootstrap"
fi

# ── 5b. Init the working-tree git repo. ──────────────────────────────────────
# The supervisor expects a real git repo (it runs `git reset --hard HEAD`,
# checkpoint tagging, etc.). Mirror setup_repo.sh: reuse trellis.git_ops.init_repo
# for the `.gitignore` (.lake/ + .trellis/ + editor/OS) + `user.name`/`user.email`
# config, pin the branch to `master`, then make the initial commit of the
# scaffolded tablet. `.trellis/` (runtime state) is gitignored, exactly as math.
echo "  Initializing working-tree git repo..."
PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$OUT_REPO" <<'PY'
import sys
from pathlib import Path

from trellis.git_ops import init_repo

repo = Path(sys.argv[1]).resolve()
init_repo(repo)
PY
# Pin the default branch name to `master` (idempotent; harmless if already so).
git -C "$OUT_REPO" branch -m master >/dev/null 2>&1 || true
git -C "$OUT_REPO" add -A
git -C "$OUT_REPO" commit -m "Initial PV tablet setup (extracted model + scaffold)" >/dev/null 2>&1 || true
echo "  Committed initial PV tablet state on branch master"

echo ""
echo "PV tablet repo prepared."
echo "  Repo:          $OUT_REPO"
echo "  Config:        $CONFIG_OUT"
if [[ "$DEFER_RUNTIME_INIT" -eq 0 && "$PREPARE_ONLY" -eq 0 ]]; then
  echo "  Runtime root:  $RUNTIME_ROOT"
else
  echo "  Runtime root:  $RUNTIME_ROOT (not created; initialize with the final trust-v1 config)"
fi

# ── 6. Optional acceptance gates (Slice 2 (a)-(f)). ──────────────────────────
if [[ "$RUN_GATE" -eq 1 ]]; then
  echo ""
  echo "Running Slice-2 acceptance gates..."
  TRELLIS_TRELLIS_KERNEL_CMD="$KERNEL_CLI_BIN" \
  PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 \
    "$SCRIPT_DIR/_setup_pv_repo_gates.py" "$RUNTIME_ROOT" "$CONFIG_OUT" "$OUT_REPO"
fi

echo ""
if [[ "$DEFER_RUNTIME_INIT" -eq 0 && "$PREPARE_ONLY" -eq 0 ]]; then
  echo "Launch with (once providers/prewarm are configured for a real run):"
  echo "  scripts/restart_configured_run.sh --no-reset $CONFIG_OUT $RUNTIME_ROOT"
else
  echo "Next: bootstrap trust-v1, then initialize this absent runtime root with:"
  echo "  scripts/trellis.sh init <trust-v1-config> $RUNTIME_ROOT"
fi
