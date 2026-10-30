#!/bin/bash
# Setup or reseed a formalization repo for trellis.
#
# Usage:
#   ./scripts/setup_repo.sh [--reset] [--resume] <repo_path> <paper_tex_path> [project_slug]
#
# Setup is organized as stages (see VIEWER_RUN_CREATION_DESIGN.md §2-§3). Each
# stage records completion in the stage ledger at <repo>/.trellis/setup_stages.json,
# and `--resume` re-enters an interrupted setup. Re-entry decides skip-vs-redo
# per stage CLASS:
#
#   always_redo  cheap, deterministic, whole-file writes — always re-executed,
#                so no half-written artifact of theirs is ever trusted.
#   verify_skip  expensive or non-re-entrant — skipped only when the ledger
#                records completion for the SAME inputs AND a validity probe on
#                the artifact itself passes. The probe, not the ledger, is what
#                makes the skip safe. The prewarm is the only such stage.
#   wipe_redo    not re-enterable and cheap to redo from nothing — the resume
#                action is "remove the output, run again". setup_repo.sh has no
#                stage of this class; the run-creation launcher's
#                runtime-root init is the one that does.
#
# Because every generated artifact is re-derived on each run, a resume that is
# given DIFFERENT inputs would silently rewrite them (a forgotten
# --challenge-targets reverts the pinned toolchain; a forgotten
# --main-result-labels widens the reviewed target set). Every input that setup
# bakes into a generated artifact is therefore pinned in the ledger and must
# match on resume — see `PINNED_*` below. --reconfigure adopts new values
# deliberately.
#
# Without --resume the behavior is byte-for-byte what it always was.

set -euo pipefail
umask 0002

TRELLIS_TMUX_SOCKET="${TRELLIS_TMUX_SOCKET:-trellis}"
export TRELLIS_TMUX_SOCKET
tmux_cmd() { tmux -L "$TRELLIS_TMUX_SOCKET" "$@"; }

usage() {
  cat <<'EOF'
Usage: ./scripts/setup_repo.sh --loogle on|off [--reset] [--resume] <repo_path> <paper_tex_path> [project_slug]

  --loogle on|off Required. Whether this host runs a local Loogle (Mathlib
                  search) server. Writes loogle.enabled into the generated
                  trellis.config.json. When off, the worker prompt omits the
                  Loogle helper; see the printed reminder about the skill files.
  --reset         Stop any existing project process and recreate the repo from scratch
  --resume        Continue an interrupted setup in an existing repo, per the
                  stage ledger at <repo>/.trellis/setup_stages.json. Cheap
                  deterministic stages re-run; the mathlib prewarm is skipped
                  only when its own on-disk artifacts still validate. Reference
                  papers are re-registered from source every time, and a stored
                  paper/refs/<id>.tex this setup did not write is refused.
                  Pass the SAME flags the repo was set up with: an input that
                  setup bakes into a generated artifact must match what the
                  ledger recorded.
                  Refuses a repo that has progressed past setup into a run.
  --reconfigure   With --resume: adopt changed inputs instead of refusing them,
                  regenerating the affected artifacts. The mathlib prewarm is
                  preserved, so this is the cheap way to correct a wrong
                  --loogle/--main-result-labels/--challenge-targets.
  --yes           Skip the target confirmation prompt
  --mathlib-build-tar path
                  Optional local tarball containing the contents of
                  .lake/packages/mathlib/.lake/build to seed prewarm.
  --main-result-labels labels
                  Comma-separated paper TeX labels to use as the human-reviewed target set.
                  If omitted, setup infers all paper theorem/corollary statements, using labels
                  when present and line ranges when not.
  --targets-json file
                  JSON array of explicit main-result targets in the kernel's
                  raw_targets wire shape: label strings and/or objects with
                  "tex_label" and/or positive "start_line"/"end_line". Explicit
                  mode — nothing is inferred, and unlabeled (line-range)
                  selections are expressible, which labels cannot do. Cannot be
                  combined with --main-result-labels (the kernel ignores raw
                  labels entirely when raw targets are present, so the label
                  list would be dropped silently).
  --env-map ALIAS=CANONICAL
                  Repeatable. Explicit alias mapping forwarded to
                  scripts/normalize_paper_envs.py --map, for papers whose
                  \newtheorem declarations carry no usable title (title-derived
                  mappings are still auto-detected without it). Baked into the
                  normalized paper copied to paper/.
  --main-result-envs list
                  Comma-separated main-result environment set, validated as a
                  subset of the canonical statement envs (theorem, lemma,
                  definition, corollary, proposition, helper). Written to
                  workflow.main_result_envs so target resolution here, every
                  load_config, and any later add-targets all resolve under one
                  pinned set. Omit for the default (theorem, corollary), which
                  writes no config key at all.
  --challenge-targets file
                  Imported challenge-target registry (challenge_targets.json from
                  scripts/import_challenge_targets.py). Copied into the run repo at
                  challenge/challenge_targets.json (read-only to workers like paper/)
                  and referenced as workflow.challenge_targets_path.
  --reference <id>=<file>[:<source_id>]
                  Repeatable. Register an additional reference paper: <file> is
                  stored at paper/refs/<id>.tex and the {id, tex_path, source_id}
                  entry is appended to workflow.reference_papers (source_id
                  defaults to <id>). Seeded into initial kernel state at init.
  repo_path       Where to create the formalization repo
  paper_tex_path  Path to the source paper .tex file
  project_slug    Optional viewer/session slug (defaults to basename(repo_path))
EOF
}

RESET=0
ADOPT_TOOLCHAIN=0
RESUME=0
RECONFIGURE=0
ASSUME_YES=0
MAIN_RESULT_LABELS=""
MAIN_RESULT_ENVS=""
TARGETS_JSON_ARG=""
ENV_MAP_SPECS=()
CHALLENGE_TARGETS=""
REFERENCE_SPECS=()
MATHLIB_BUILD_TAR="${MATHLIB_BUILD_TAR:-}"
LOOGLE_SETTING=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --reset|--force)
      RESET=1
      shift
      ;;
    # Consent to --reset changing an existing run's Lean toolchain. Without
    # it, a disagreement between the repo's pin and MATHLIB_TOOLCHAIN is a
    # hard error rather than a silent migration.
    --adopt-toolchain)
      ADOPT_TOOLCHAIN=1
      shift
      ;;
    --resume)
      RESUME=1
      shift
      ;;
    --reconfigure)
      RECONFIGURE=1
      shift
      ;;
    --loogle)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --loogle requires an argument: on or off" >&2
        exit 1
      fi
      case "$2" in
        on|off) LOOGLE_SETTING="$2" ;;
        *) echo "ERROR: --loogle must be 'on' or 'off', got: $2" >&2; exit 1 ;;
      esac
      shift 2
      ;;
    --yes|-y)
      ASSUME_YES=1
      shift
      ;;
    --main-result-labels)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --main-result-labels requires a comma-separated argument" >&2
        exit 1
      fi
      MAIN_RESULT_LABELS="$2"
      shift 2
      ;;
    --main-result-envs)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --main-result-envs requires a comma-separated argument" >&2
        exit 1
      fi
      MAIN_RESULT_ENVS="$2"
      shift 2
      ;;
    --targets-json)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --targets-json requires a path argument" >&2
        exit 1
      fi
      TARGETS_JSON_ARG="$2"
      shift 2
      ;;
    --env-map)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --env-map requires ALIAS=CANONICAL" >&2
        exit 1
      fi
      ENV_MAP_SPECS+=("$2")
      shift 2
      ;;
    --challenge-targets)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --challenge-targets requires a path argument" >&2
        exit 1
      fi
      CHALLENGE_TARGETS="$2"
      shift 2
      ;;
    --reference)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --reference requires <id>=<file>[:<source_id>]" >&2
        exit 1
      fi
      REFERENCE_SPECS+=("$2")
      shift 2
      ;;
    --mathlib-build-tar)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --mathlib-build-tar requires a path argument" >&2
        exit 1
      fi
      MATHLIB_BUILD_TAR="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      echo "ERROR: Unknown option: $1" >&2
      usage >&2
      exit 1
      ;;
    *)
      break
      ;;
  esac
done

if [ $# -lt 2 ]; then
  usage >&2
  exit 1
fi

if [ -z "$LOOGLE_SETTING" ]; then
  echo "ERROR: --loogle on|off is required. Does this host run a local Loogle" >&2
  echo "       (Mathlib search) server? Pass --loogle on if so, otherwise --loogle off." >&2
  exit 1
fi
if [ "$LOOGLE_SETTING" = "on" ]; then LOOGLE_ENABLED_JSON="true"; else LOOGLE_ENABLED_JSON="false"; fi

REPO="$1"
PAPER="$2"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO="$(python3 - "$REPO" <<'PY'
from pathlib import Path
import sys

print(Path(sys.argv[1]).resolve())
PY
)"
PAPER="$(python3 - "$PAPER" <<'PY'
from pathlib import Path
import sys

print(Path(sys.argv[1]).resolve())
PY
)"
DEFAULT_SLUG="$(basename "$REPO" | sed -E 's/_tablets?$//')"
PROJECT_SLUG="${3:-${PROJECT_SLUG:-$DEFAULT_SLUG}}"
DEFAULT_CONFIG_TEMPLATE="$SOURCE_ROOT/examples/trellis.config.json"
CONFIG_TEMPLATE="${CONFIG_TEMPLATE:-$DEFAULT_CONFIG_TEMPLATE}"
if [[ -z "${POLICY_TEMPLATE:-}" ]]; then
  if [[ "$CONFIG_TEMPLATE" == *.config.json ]]; then
    POLICY_TEMPLATE="${CONFIG_TEMPLATE%.config.json}.policy.json"
  else
    POLICY_TEMPLATE="${CONFIG_TEMPLATE%.json}.policy.json"
  fi
fi
CONFIG_OUT="$REPO/trellis.config.json"
POLICY_OUT="$REPO/trellis.policy.json"
STATIC_OUT="${STATIC_OUT:-$HOME/trellis-web}"
PROJECT_STATIC_DIR="$STATIC_OUT/$PROJECT_SLUG"
BURST_USER="${BURST_USER:-$(id -un)}"
BURST_GROUP="${BURST_GROUP:-$(id -gn)}"
ENV_MATHLIB_TOOLCHAIN="${MATHLIB_TOOLCHAIN:-}"
ENV_MATHLIB_REV="${MATHLIB_REV:-}"
# Default pin: the mathlib `v4.33.0` tag (2026-08-10) and the toolchain that
# tag itself pins. A tagged mathlib release is a tested mathlib+toolchain
# pairing with a reliable `lake exe cache get`, which an rc or a bare mathlib
# HEAD is not — and a run lives for months, so pinning a soon-superseded rc
# costs more than trailing the tag.
MATHLIB_TOOLCHAIN="${MATHLIB_TOOLCHAIN:-leanprover/lean4:v4.33.0}"
MATHLIB_REV="${MATHLIB_REV:-db584cd6d46c92f209a44c0f1c829460d327499d}"
# A challenge spec carries the benchmark's recorded toolchain pins, and the
# submission is validated against the benchmark's own workspace — the spec
# pins are authoritative. An explicitly exported pin that disagrees is a
# configuration error, caught here instead of at export time.
if [ -n "$CHALLENGE_TARGETS" ] && [ -f "$CHALLENGE_TARGETS" ]; then
  SPEC_MATHLIB_TOOLCHAIN="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["toolchain"]["MATHLIB_TOOLCHAIN"])' "$CHALLENGE_TARGETS")"
  SPEC_MATHLIB_REV="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["toolchain"]["MATHLIB_REV"])' "$CHALLENGE_TARGETS")"
  if [ -n "$ENV_MATHLIB_TOOLCHAIN" ] && [ "$ENV_MATHLIB_TOOLCHAIN" != "$SPEC_MATHLIB_TOOLCHAIN" ]; then
    echo "ERROR: MATHLIB_TOOLCHAIN=$ENV_MATHLIB_TOOLCHAIN disagrees with the challenge spec's $SPEC_MATHLIB_TOOLCHAIN" >&2
    exit 1
  fi
  if [ -n "$ENV_MATHLIB_REV" ] && [ "$ENV_MATHLIB_REV" != "$SPEC_MATHLIB_REV" ]; then
    echo "ERROR: MATHLIB_REV=$ENV_MATHLIB_REV disagrees with the challenge spec's $SPEC_MATHLIB_REV" >&2
    exit 1
  fi
  MATHLIB_TOOLCHAIN="$SPEC_MATHLIB_TOOLCHAIN"
  MATHLIB_REV="$SPEC_MATHLIB_REV"
  echo "[setup] Toolchain pinned by challenge spec: $MATHLIB_TOOLCHAIN / $MATHLIB_REV"
fi
# Post-bwrap-only: the burst runs as the operator with a dedicated fake home
# (the per-burst home is materialized under <runtime>/burst-homes/ at run time;
# this default is only used by the setup-time preflight + prewarm). Must exist
# and be writable, so it is created below before the bwrap preflight.
BURST_HOME="${BURST_HOME:-$HOME/.cache/trellis-burst-home}"
mkdir -p "$BURST_HOME"
ELAN_HOME="${ELAN_HOME:-$HOME/.elan}"
# Default PATH for the in-bwrap validation + prewarm steps. elan installs lake
# to ~/.elan/bin (see INSTALLATION.md §2c), which is not on the system PATH, so
# append it to the default — otherwise `lake env lean` validation fails
# "command not found". Provider CLIs live in a user-local npm prefix whose
# location varies; export BURST_PATH explicitly to add that dir (documented in
# INSTALLATION.md §4). An explicit BURST_PATH override is respected as-is.
BURST_PATH="${BURST_PATH:-$HOME/.elan/bin:/usr/local/bin:/usr/bin:/bin}"
SETUP_SCRATCH_ROOT="${SETUP_SCRATCH_ROOT:-$SOURCE_ROOT/.trellis/setup_repo_tmp}"
if [[ -n "$MATHLIB_BUILD_TAR" ]]; then
  MATHLIB_BUILD_TAR="$(python3 - "$MATHLIB_BUILD_TAR" <<'PY'
from pathlib import Path
import sys

print(Path(sys.argv[1]).resolve())
PY
)"
fi

if [ ! -f "$PAPER" ]; then
  echo "ERROR: Paper not found: $PAPER" >&2
  exit 1
fi
# Validate the explicit-target/env-map inputs up front (fail before any repo
# mutation, like the --reference specs below). The canonical-set validation of
# --env-map targets and --main-result-envs entries lives with their owners
# (normalize_paper_envs.py --map in S1, trellis.config in S2) — both run
# before the first repo write, so a bad value still cannot leave a repo behind.
if [[ -n "$TARGETS_JSON_ARG" ]]; then
  TARGETS_JSON_ARG="$(python3 - "$TARGETS_JSON_ARG" <<'PY'
from pathlib import Path
import sys

print(Path(sys.argv[1]).resolve())
PY
)"
  if [ ! -f "$TARGETS_JSON_ARG" ]; then
    echo "ERROR: --targets-json file not found: $TARGETS_JSON_ARG" >&2
    exit 1
  fi
  if [ -n "$MAIN_RESULT_LABELS" ]; then
    echo "ERROR: --targets-json and --main-result-labels are contradictory: with" >&2
    echo "       explicit raw targets the kernel ignores raw labels entirely, so" >&2
    echo "       the label list would be dropped silently. Pass one or the other." >&2
    exit 1
  fi
fi
for spec in ${ENV_MAP_SPECS[@]+"${ENV_MAP_SPECS[@]}"}; do
  if [[ "$spec" != ?*=?* ]]; then
    echo "ERROR: --env-map expects ALIAS=CANONICAL, got: $spec" >&2
    exit 1
  fi
done
# One canonical serialization of the env-map specs, for the ledger pin and the
# normalize stage's inputs hash. Order-preserving: later --map specs override
# earlier ones in normalize_paper_envs.py, so order is part of the input.
ENV_MAP_JOINED=""
for spec in ${ENV_MAP_SPECS[@]+"${ENV_MAP_SPECS[@]}"}; do
  ENV_MAP_JOINED="${ENV_MAP_JOINED:+$ENV_MAP_JOINED;}$(printf '%s' "$spec" | tr -d '[:space:]')"
done
# Parse a --reference spec `<id>=<file>[:<source_id>]` into
# REF_SPEC_ID / REF_SPEC_FILE / REF_SPEC_SOURCE. The optional
# `:<source_id>` is split from the RIGHT, and a full remainder that
# exists as a file wins outright — so file paths containing ':' work
# both with and without an explicit source_id, and a bad spec fails
# loudly instead of silently truncating at the first colon.
parse_reference_spec() {
  local spec="$1"
  REF_SPEC_ID="${spec%%=*}"
  local rest="${spec#*=}"
  if [[ -z "$REF_SPEC_ID" || "$REF_SPEC_ID" == "$spec" || -z "$rest" ]]; then
    echo "ERROR: --reference expects <id>=<file>[:<source_id>], got: $spec" >&2
    return 1
  fi
  if [ -f "$rest" ]; then
    REF_SPEC_FILE="$rest"
    REF_SPEC_SOURCE="$REF_SPEC_ID"
  elif [[ "$rest" == *:* ]]; then
    REF_SPEC_FILE="${rest%:*}"
    REF_SPEC_SOURCE="${rest##*:}"
  else
    REF_SPEC_FILE="$rest"
    REF_SPEC_SOURCE="$REF_SPEC_ID"
  fi
  if [ ! -f "$REF_SPEC_FILE" ]; then
    echo "ERROR: --reference file not found: $REF_SPEC_FILE (spec: $spec). When the remainder after '<id>=' is not an existing file, the text after the LAST ':' is taken as <source_id>." >&2
    return 1
  fi
}

# Validate --reference specs up front (fail before any repo mutation).
for spec in "${REFERENCE_SPECS[@]:-}"; do
  [[ -z "$spec" ]] && continue
  parse_reference_spec "$spec" || exit 1
done
if [ ! -f "$CONFIG_TEMPLATE" ]; then
  echo "ERROR: Config template not found: $CONFIG_TEMPLATE" >&2
  exit 1
fi
# Backend the tablet targets, read from the config template's
# `workflow.default_target` (mirrors the kernel + bridge resolution).
# Defaults to `lean` when absent/unreadable, so a Lean config is unaffected.
DEFAULT_TARGET="$(python3 -c 'import json,sys
try:
    d=json.load(open(sys.argv[1]))
    v=(d.get("workflow") or {}).get("default_target")
    print(v.strip() if isinstance(v,str) and v.strip() else "lean")
except Exception:
    print("lean")' "$CONFIG_TEMPLATE")"
# The env set target resolution runs under. The CLI flag wins; otherwise a
# template that carries workflow.main_result_envs pins it, so the S2 resolve
# and the config S6 generates from that template cannot disagree about the env
# set; otherwise empty ⇒ the kernel default AND no key written to the config
# (the absent-key default keeps a no-flag setup byte-identical to before the
# knob existed).
TEMPLATE_MAIN_RESULT_ENVS="$(python3 -c 'import json,sys
try:
    d=json.load(open(sys.argv[1]))
    v=(d.get("workflow") or {}).get("main_result_envs")
    print(",".join(str(x).strip() for x in v) if isinstance(v,list) else "")
except Exception:
    print("")' "$CONFIG_TEMPLATE")"
EFFECTIVE_MAIN_RESULT_ENVS="${MAIN_RESULT_ENVS:-$TEMPLATE_MAIN_RESULT_ENVS}"
if [[ -n "$MATHLIB_BUILD_TAR" ]] && [ ! -f "$MATHLIB_BUILD_TAR" ]; then
  echo "ERROR: Mathlib build tarball not found: $MATHLIB_BUILD_TAR" >&2
  exit 1
fi
if [ "$RECONFIGURE" -eq 1 ] && [ "$RESUME" -ne 1 ]; then
  echo "ERROR: --reconfigure only means something together with --resume." >&2
  exit 1
fi
if [ "$RESET" -eq 1 ] && [ "$RESUME" -eq 1 ]; then
  echo "ERROR: --reset and --resume are contradictory: one rebuilds from nothing," >&2
  echo "       the other continues what is already there. Pick one." >&2
  exit 1
fi
if [ -e "$REPO" ] && [ "$RESET" -ne 1 ] && [ "$RESUME" -ne 1 ]; then
  echo "ERROR: Repo path already exists. Re-run with --resume to continue an" >&2
  echo "       interrupted setup, or --reset to recreate it from scratch." >&2
  exit 1
fi

# Exclusive per-repo lock, held for the whole run through a dedicated fd.
# Setup rewrites fixed-name temporaries (trellis.config.json.setup.tmp, the
# ledger's, the mathlib seed's partial directory) and sweeps stale ones; two
# invocations against one repo would interleave those and could rename a tree
# the other is still writing. They would also race the git index. Serializing
# is not enough — a second setup has nothing useful to wait for — so refuse.
# The lock lives BESIDE the repo: it must exist before the repo does and
# survive `--reset`'s `rm -rf`.
SETUP_LOCK_FILE="$(dirname "$REPO")/.$(basename "$REPO").setup.lock"
mkdir -p "$(dirname "$REPO")"
exec {SETUP_LOCK_FD}>"$SETUP_LOCK_FILE"
if ! flock -n "$SETUP_LOCK_FD"; then
  echo "ERROR: another setup_repo.sh is already running for this repo." >&2
  echo "       Lock: $SETUP_LOCK_FILE" >&2
  echo "       Wait for it to finish (or kill it) before re-running." >&2
  exit 1
fi

PAPER_NAME="$(basename "$PAPER")"
mkdir -p "$SETUP_SCRATCH_ROOT"
TARGETS_JSON="$(mktemp "$SETUP_SCRATCH_ROOT/targets.XXXXXX.json")"
TARGETS_PREVIEW="$(mktemp "$SETUP_SCRATCH_ROOT/targets-preview.XXXXXX.txt")"
NORMALIZED_PAPER="$(mktemp "$SETUP_SCRATCH_ROOT/${PAPER_NAME%.tex}.normalized.XXXXXX.tex")"
# Throwaway working directory for the S4 bwrap preflight. The probe hands its
# work_dir to `trellis.sandbox.wrap_command`, which treats that directory AS A
# REPO: it materializes the whole worker writable-path set inside it (Tablet/,
# reference/, .trellis/{logs,chats,...}, .lake/build) so bwrap has something to
# bind. So the probe's work_dir must be a directory setup owns and discards —
# never a real one. See stage_sandbox_probe.
SANDBOX_PROBE_DIR="$(mktemp -d "$SETUP_SCRATCH_ROOT/sandbox-probe.XXXXXX")"
cleanup_preview_files() {
  rm -f "$TARGETS_JSON" "$TARGETS_PREVIEW" "$NORMALIZED_PAPER"
  rm -rf "$SANDBOX_PROBE_DIR"
}
trap cleanup_preview_files EXIT

# ---------------------------------------------------------------------------
# Stage ledger
#
# One record per stage at <repo>/.trellis/setup_stages.json:
#   {stage, status, class, inputs_sha, finished_ts, output_sha?}
# written atomically (tmp+rename) only AFTER the stage's outputs are durable.
# `inputs_sha` is a sha256 over the stage's declared inputs — file CONTENT
# where hashing it is cheap, and path+size+mtime for the one input where it is
# not (the multi-gigabyte mathlib build tarball, whose content hash would cost
# minutes on every run). A changed input invalidates that stage. A corrupt or
# absent ledger is treated as absent — the worst case is redoing work, never
# trusting it (design §7 row 27).
#
# The ledger also carries `pinned_inputs`: the invocation values setup bakes
# into generated artifacts, which must match on resume (see PINNED_* below).
#
# The ledger lives inside the repo, which does not exist yet while the first
# few stages run. Records are therefore kept in memory and flushed on every
# update once <repo>/.trellis exists; the flush that follows the skeleton
# stage lands the earlier records too. Creating the repo earlier just to hold
# a ledger would change hand-operator behavior (a declined confirmation prompt
# would start leaving a repo directory behind), which is not allowed.
# ---------------------------------------------------------------------------
LEDGER_PATH="$REPO/.trellis/setup_stages.json"
declare -A LEDGER_STATUS=()
declare -A LEDGER_CLASS=()
declare -A LEDGER_SHA=()
declare -A LEDGER_TS=()
declare -A LEDGER_OUT=()
LEDGER_STAGES=()
declare -A PINNED_RECORDED=()
declare -A PINNED_CURRENT=()
PINNED_KEYS=()

sha256_file() {
  if [ -f "$1" ]; then
    sha256sum -- "$1" | cut -d' ' -f1
  else
    echo "absent"
  fi
}

sha256_args() {
  printf '%s\0' "$@" | sha256sum | cut -d' ' -f1
}

ledger_reset() {
  LEDGER_STATUS=()
  LEDGER_CLASS=()
  LEDGER_SHA=()
  LEDGER_TS=()
  LEDGER_OUT=()
  LEDGER_STAGES=()
  PINNED_RECORDED=()
}

ledger_load() {
  ledger_reset
  [ -f "$LEDGER_PATH" ] || return 0
  local kind id status cls sha ts out
  while IFS=$'\t' read -r kind id status cls sha ts out; do
    case "$kind" in
      pinned)
        [ -n "$id" ] || continue
        PINNED_RECORDED["$id"]="$status"
        ;;
      stage)
        [ -n "$id" ] || continue
        LEDGER_STAGES+=("$id")
        LEDGER_STATUS["$id"]="$status"
        LEDGER_CLASS["$id"]="$cls"
        LEDGER_SHA["$id"]="$sha"
        LEDGER_TS["$id"]="$ts"
        LEDGER_OUT["$id"]="$out"
        ;;
    esac
  done < <(python3 - "$LEDGER_PATH" <<'PY'
import json
import sys
from pathlib import Path

try:
    data = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
except Exception:
    # Corrupt or half-written ledger: treat as absent.
    raise SystemExit(0)
if not isinstance(data, dict):
    raise SystemExit(0)
pinned = data.get("pinned_inputs")
if isinstance(pinned, dict):
    for key, value in pinned.items():
        fields = ["pinned", str(key), str(value), "", "", "", ""]
        if any("\t" in field or "\n" in field for field in fields):
            continue
        print("\t".join(fields))
stages = data.get("stages")
if not isinstance(stages, dict):
    raise SystemExit(0)
for stage_id, record in stages.items():
    if not isinstance(record, dict):
        continue
    fields = [
        "stage",
        str(stage_id),
        str(record.get("status", "")),
        str(record.get("class", "")),
        str(record.get("inputs_sha", "")),
        str(record.get("finished_ts", "")),
        str(record.get("output_sha", "") or ""),
    ]
    if any("\t" in field or "\n" in field for field in fields):
        continue
    print("\t".join(fields))
PY
  )
}

ledger_flush() {
  # No-op until the repo's .trellis dir exists; the next flush writes
  # everything accumulated so far.
  [ -d "$REPO/.trellis" ] || return 0
  local args=() id
  for id in ${LEDGER_STAGES[@]+"${LEDGER_STAGES[@]}"}; do
    args+=("$(printf 'stage\t%s\t%s\t%s\t%s\t%s\t%s' \
      "$id" "${LEDGER_STATUS[$id]}" "${LEDGER_CLASS[$id]}" \
      "${LEDGER_SHA[$id]}" "${LEDGER_TS[$id]}" "${LEDGER_OUT[$id]}")")
  done
  for id in ${PINNED_KEYS[@]+"${PINNED_KEYS[@]}"}; do
    args+=("$(printf 'pinned\t%s\t%s' "$id" "${PINNED_CURRENT[$id]}")")
  done
  python3 - "$LEDGER_PATH" ${args[@]+"${args[@]}"} <<'PY'
import json
import os
import sys
from pathlib import Path

ledger_path = Path(sys.argv[1])
stages = {}
pinned = {}
for raw in sys.argv[2:]:
    fields = raw.split("\t")
    if fields[0] == "pinned":
        pinned[fields[1]] = fields[2]
        continue
    _, stage_id, status, cls, inputs_sha, finished_ts, output_sha = fields
    record = {
        "stage": stage_id,
        "status": status,
        "class": cls,
        "inputs_sha": inputs_sha,
        "finished_ts": finished_ts,
    }
    if output_sha:
        record["output_sha"] = output_sha
    stages[stage_id] = record

tmp_path = ledger_path.with_name(ledger_path.name + ".setup.tmp")
tmp_path.write_text(
    json.dumps({"version": 1, "pinned_inputs": pinned, "stages": stages}, indent=2)
    + "\n",
    encoding="utf-8",
)
os.replace(tmp_path, ledger_path)
PY
}

ledger_record() {
  local id="$1" cls="$2" status="$3" sha="$4" out="${5:-}"
  if [ -z "${LEDGER_STATUS[$id]+set}" ]; then
    LEDGER_STAGES+=("$id")
  fi
  LEDGER_STATUS["$id"]="$status"
  LEDGER_CLASS["$id"]="$cls"
  LEDGER_SHA["$id"]="$sha"
  LEDGER_TS["$id"]="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  LEDGER_OUT["$id"]="$out"
  ledger_flush
}

ledger_is_done() {
  # done for exactly these inputs?
  local id="$1" sha="$2"
  [ "${LEDGER_STATUS[$id]:-}" = "done" ] && [ "${LEDGER_SHA[$id]:-}" = "$sha" ]
}

probe_true() { return 0; }

# run_stage <stage_id> <class> <inputs_sha> <probe_fn> <body_fn>
run_stage() {
  local id="$1" cls="$2" sha="$3" probe="$4" body="$5"
  case "$cls" in
    always_redo)
      ;;
    verify_skip)
      if [ "$RESUME" -eq 1 ] && ledger_is_done "$id" "$sha" && "$probe"; then
        echo "  [resume] skip $id (recorded complete for these inputs; artifact probe passed)"
        return 0
      fi
      ;;
    wipe_redo)
      # Not re-enterable: discard whatever is there and rebuild from nothing.
      if [ "$RESUME" -eq 1 ] && [ -n "${LEDGER_STATUS[$id]:-}" ]; then
        echo "  [resume] wipe+redo $id"
      fi
      "${body}_wipe"
      ;;
    *)
      echo "ERROR: unknown stage class '$cls' for stage $id" >&2
      exit 1
      ;;
  esac
  ledger_record "$id" "$cls" "running" "$sha"
  "$body"
  ledger_record "$id" "$cls" "done" "$sha" "${STAGE_OUTPUT_SHA:-}"
  STAGE_OUTPUT_SHA=""
}

STAGE_OUTPUT_SHA=""
ledger_load

# ---------------------------------------------------------------------------
# Pinned inputs — everything an invocation supplies that setup BAKES INTO a
# generated artifact, and which must therefore not change silently under a
# resume. The list is complete by construction: it is the enumeration of every
# write the generating stages make, and what determines each one.
#
#   S5  paper/<name>        <- the paper's BYTES (deliberately not pinned: a
#                              corrected paper is exactly what re-resolving is
#                              for), its NAME, which the config records and
#                              whose change would strand a second tracked copy,
#                              and the --env-map rewrites S1 bakes into the
#                              normalized copy
#       challenge/…json     <- --challenge-targets
#       lakefile.lean       <- MATHLIB_REV       (env, or the challenge spec)
#       lean-toolchain      <- MATHLIB_TOOLCHAIN (env, or the challenge spec)
#       FILESPEC + rubrics  <- the trellis checkout, not an invocation input
#       Preamble/AXIOMS/…   <- constants
#   S6  trellis.config.json <- CONFIG_TEMPLATE; the resolved targets (paper +
#                              --main-result-labels or --targets-json, under
#                              --main-result-envs); workflow.main_result_envs
#                              <- --main-result-envs; --loogle; the slug; burst
#                              user/group/home; the challenge path
#       trellis.policy.json <- POLICY_TEMPLATE
#   S7  paper/refs/<id>.tex <- --reference (also guarded per id, further down)
#   S12 viewer route        <- STATIC_OUT, the slug
#
# --mathlib-build-tar is deliberately absent: it seeds .lake only, which is
# gitignored, unpinned build state that lake re-validates. It is an input to
# the prewarm stage's own inputs_sha instead.
# ---------------------------------------------------------------------------
pin_input() {
  local key="$1" value="$2"
  PINNED_KEYS+=("$key")
  PINNED_CURRENT["$key"]="$(printf '%s' "$value" | tr -d '\t\n')"
}

pin_input loogle "$LOOGLE_SETTING"
pin_input main_result_labels "$(printf '%s' "$MAIN_RESULT_LABELS" | tr -d '[:space:]')"
pin_input main_result_envs "$(printf '%s' "$MAIN_RESULT_ENVS" | tr -d '[:space:]')"
pin_input targets_json "$(sha256_file "$TARGETS_JSON_ARG")"
pin_input env_map "$ENV_MAP_JOINED"
pin_input challenge_targets "$(sha256_file "$CHALLENGE_TARGETS")"
pin_input mathlib_rev "$MATHLIB_REV"
pin_input mathlib_toolchain "$MATHLIB_TOOLCHAIN"
pin_input config_template "$(sha256_file "$CONFIG_TEMPLATE")"
pin_input policy_template "$(sha256_file "$POLICY_TEMPLATE")"
pin_input paper_name "$PAPER_NAME"
pin_input project_slug "$PROJECT_SLUG"
pin_input burst_user "$BURST_USER"
pin_input burst_group "$BURST_GROUP"
pin_input burst_home "$BURST_HOME"
pin_input static_out "$STATIC_OUT"
pin_input reference_ids "$(
  printf '%s\n' ${REFERENCE_SPECS[@]+"${REFERENCE_SPECS[@]}"} \
    | sed -e 's/=.*$//' | grep -v '^$' | sort | tr '\n' ',' || true
)"

if [ "$RESUME" -eq 1 ] && [ "${#PINNED_RECORDED[@]}" -gt 0 ]; then
  PINNED_MISMATCHES=()
  for pinned_key in "${PINNED_KEYS[@]}"; do
    if [ -n "${PINNED_RECORDED[$pinned_key]+set}" ] \
       && [ "${PINNED_RECORDED[$pinned_key]}" != "${PINNED_CURRENT[$pinned_key]}" ]; then
      PINNED_MISMATCHES+=("$pinned_key: recorded '${PINNED_RECORDED[$pinned_key]}', now '${PINNED_CURRENT[$pinned_key]}'")
    fi
  done
  if [ "${#PINNED_MISMATCHES[@]}" -gt 0 ]; then
    if [ "$RECONFIGURE" -eq 1 ]; then
      echo "  --reconfigure: adopting changed inputs and regenerating what they feed:"
      for pinned_line in "${PINNED_MISMATCHES[@]}"; do
        echo "    $pinned_line"
      done
    else
      echo "ERROR: --resume was given, but this invocation's inputs differ from the" >&2
      echo "       ones recorded for this repo:" >&2
      for pinned_line in "${PINNED_MISMATCHES[@]}"; do
        echo "         $pinned_line" >&2
      done
      echo "       Setup regenerates lakefile.lean, lean-toolchain, trellis.config.json" >&2
      echo "       and the viewer route from these on EVERY run, so continuing would" >&2
      echo "       silently rewrite them — a dropped --challenge-targets unpins the" >&2
      echo "       toolchain, a dropped --main-result-labels widens the reviewed target" >&2
      echo "       set. Re-run with the recorded values, or pass --reconfigure to adopt" >&2
      echo "       the new ones deliberately (the mathlib prewarm is kept either way)." >&2
      exit 1
    fi
  fi
fi

# --resume is for an interrupted SETUP. Pointed at a repo a run has since moved
# into, it would overwrite worker-authored files (Tablet/Preamble.lean,
# APPROVED_AXIOMS.json, HUMAN_INPUT.md are unconditional whole-file writes) and
# commit whatever it found. The unconditional "repo exists ⇒ refuse" was the
# old guard against that; this is its replacement.
repo_has_run_state() {
  if [ -e "$REPO/.trellis/supervisor" ]; then
    return 0
  fi
  if [ -n "$(ls -A "$REPO/.trellis/checkpoints" 2>/dev/null)" ]; then
    return 0
  fi
  local lean_file
  for lean_file in "$REPO"/Tablet/*.lean; do
    if [ -e "$lean_file" ] && [ "$(basename "$lean_file")" != "Preamble.lean" ]; then
      return 0
    fi
  done
  return 1
}

if [ "$RESUME" -eq 1 ] && [ -e "$REPO" ] && repo_has_run_state; then
  echo "ERROR: $REPO has progressed past setup into a run (tablet nodes," >&2
  echo "       checkpoints, or a supervisor workspace are present). --resume" >&2
  echo "       rewrites generated files and commits the result, so it refuses" >&2
  echo "       to touch a live or completed run." >&2
  echo "       To restart a configured run use scripts/restart_configured_run.sh." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# S1 — normalize \newtheorem aliases (always_redo, pure)
# ---------------------------------------------------------------------------
# Normalize \newtheorem aliases (e.g. theo->theorem, cor->corollary) to the
# canonical envs the kernel paper parser looks for. A paper already using
# canonical envs is passed through unchanged. The normalized copy is what we
# both pass to the kernel target resolver AND copy into the repo as paper/.
stage_normalize_envs() {
  local map_args=() spec
  for spec in ${ENV_MAP_SPECS[@]+"${ENV_MAP_SPECS[@]}"}; do
    map_args+=(--map "$spec")
  done
  python3 "$SCRIPT_DIR/normalize_paper_envs.py" \
    ${map_args[@]+"${map_args[@]}"} "$PAPER" "$NORMALIZED_PAPER"
  PAPER="$NORMALIZED_PAPER"
}

run_stage normalize_envs always_redo \
  "$(sha256_args "$(sha256_file "$PAPER")" "$(sha256_file "$SCRIPT_DIR/normalize_paper_envs.py")" "$ENV_MAP_JOINED")" \
  probe_true stage_normalize_envs

echo "Setting up repo at: $REPO"
echo "  Paper: $PAPER ($PAPER_NAME)"
echo "  Project slug: $PROJECT_SLUG"
echo "  Burst user: $BURST_USER"
echo "  Burst group: $BURST_GROUP"
echo "  Config out: $CONFIG_OUT"
if [[ -n "$MAIN_RESULT_LABELS" ]]; then
  echo "  Main-result labels: $MAIN_RESULT_LABELS"
fi
if [[ -n "$TARGETS_JSON_ARG" ]]; then
  echo "  Explicit targets: $TARGETS_JSON_ARG"
fi
if [[ -n "$EFFECTIVE_MAIN_RESULT_ENVS" ]]; then
  echo "  Main-result envs: $EFFECTIVE_MAIN_RESULT_ENVS"
fi
if [[ -n "$ENV_MAP_JOINED" ]]; then
  echo "  Env map: $ENV_MAP_JOINED"
fi
if [[ -n "$MATHLIB_BUILD_TAR" ]]; then
  echo "  Mathlib build tar: $MATHLIB_BUILD_TAR"
fi
if [ "$RESUME" -eq 1 ]; then
  echo "  Resume: on (stage ledger $LEDGER_PATH)"
fi

# ---------------------------------------------------------------------------
# S2 — kernel main-result target resolve (always_redo, pure)
# ---------------------------------------------------------------------------
stage_resolve_targets() {
PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$PAPER" "$MAIN_RESULT_LABELS" "$TARGETS_JSON" "$TARGETS_PREVIEW" "$TARGETS_JSON_ARG" "$EFFECTIVE_MAIN_RESULT_ENVS" <<'PY'
import json
import sys
from pathlib import Path

from trellis.config import (
    ConfigError,
    format_main_result_target,
    normalize_main_result_envs,
    resolve_main_result_targets_via_kernel,
)

paper_path = Path(sys.argv[1]).resolve()
raw_main_result_labels = sys.argv[2]
targets_json = Path(sys.argv[3]).resolve()
targets_preview = Path(sys.argv[4]).resolve()
raw_targets_path = sys.argv[5]
raw_main_result_envs = sys.argv[6]

labels = []
if raw_main_result_labels.strip():
    seen = set()
    for raw_label in raw_main_result_labels.split(","):
        label = raw_label.strip()
        if not label or label in seen:
            continue
        seen.add(label)
        labels.append(label)

# --main-result-envs (or a template-carried workflow.main_result_envs):
# normalized + validated by the same helper load_config uses, so a bad entry
# fails here — before any repo write — with the canonical-set error message.
envs = None
if raw_main_result_envs.strip():
    try:
        envs = normalize_main_result_envs(
            [entry for entry in raw_main_result_envs.split(",") if entry.strip()],
            "--main-result-envs",
        )
    except ConfigError as exc:
        raise SystemExit(str(exc)) from exc

# --targets-json: explicit raw_targets, in the kernel's wire shape. Shape is
# checked here because the kernel SILENTLY drops an entry that normalizes to
# nothing — acceptable for config re-loads, not for an explicit selection.
raw_targets = None
if raw_targets_path:
    try:
        raw_targets = json.loads(Path(raw_targets_path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise SystemExit(f"--targets-json {raw_targets_path} is unreadable: {exc}") from exc
    if not isinstance(raw_targets, list) or not raw_targets:
        raise SystemExit(
            f"--targets-json {raw_targets_path} must be a non-empty JSON array of targets"
        )
    for index, entry in enumerate(raw_targets, start=1):
        if isinstance(entry, str) and entry.strip():
            continue
        if isinstance(entry, dict):
            label_value = entry.get("tex_label")
            has_label = isinstance(label_value, str) and label_value.strip()
            start_line = entry.get("start_line")
            end_line = entry.get("end_line")
            has_lines = (
                isinstance(start_line, int)
                and isinstance(end_line, int)
                and start_line > 0
                and end_line > 0
            )
            if has_label or has_lines:
                continue
        raise SystemExit(
            f"--targets-json entry {index} is not a target: expected a label string or "
            f'an object with "tex_label" and/or positive "start_line"/"end_line", '
            f"got: {json.dumps(entry)}"
        )

try:
    resolved = resolve_main_result_targets_via_kernel(
        paper_path=paper_path,
        raw_targets=raw_targets,
        raw_labels=labels or None,
        main_result_envs=envs,
    )
except ConfigError as exc:
    raise SystemExit(str(exc)) from exc

targets = resolved["targets"]
available_labels = resolved["available_labels"]
preview = resolved["preview"]
preview_lines = ["Resolved main-result targets:"]

if not targets:
    preview_lines.append("(none)")
else:
    for idx, target in enumerate(targets, start=1):
        if idx > len(preview) or not isinstance(preview[idx - 1], dict):
            raise SystemExit(
                f"Could not locate paper text for resolved main-result target {format_main_result_target(target)}."
            )
        preview_entry = preview[idx - 1]
        target_header = (
            f"{idx}. {format_main_result_target(target)} "
            f"[{str(preview_entry.get('env', '') or '').strip()}]"
        )
        preview_lines.append(target_header)
        preview_lines.append(str(preview_entry.get("text", "") or "").strip())
        preview_lines.append("")

targets_json.write_text(
    json.dumps(
        {
            "labels": labels,
            "targets": targets,
            "available_labels": available_labels,
            # The env set these targets were resolved under (normalized), or
            # None for the kernel default. S6 writes exactly this value to
            # workflow.main_result_envs, so the generated config can never
            # record a different set than the one the preview was scanned with.
            "main_result_envs": envs,
        },
        indent=2,
    )
    + "\n",
    encoding="utf-8",
)
targets_preview.write_text("\n".join(preview_lines).rstrip() + "\n", encoding="utf-8")
PY
}

run_stage resolve_targets always_redo \
  "$(sha256_args "$(sha256_file "$PAPER")" "$MAIN_RESULT_LABELS" \
     "$(sha256_file "$TARGETS_JSON_ARG")" "$EFFECTIVE_MAIN_RESULT_ENVS")" \
  probe_true stage_resolve_targets

# ---------------------------------------------------------------------------
# S3 — human confirmation of the resolved targets (always_redo; tty-only)
#
# Re-asked on --resume: the resolve above ran again, so the operator is
# confirming what this invocation actually resolved, not what an earlier one
# did. --yes skips it exactly as before.
# ---------------------------------------------------------------------------
stage_confirm_targets() {
  echo ""
  cat "$TARGETS_PREVIEW"
  echo ""
  if [ "$ASSUME_YES" -eq 0 ]; then
    if [ ! -t 0 ]; then
      echo "ERROR: setup requires target confirmation. Re-run with --yes in non-interactive mode." >&2
      exit 1
    fi
    read -r -p "Proceed with these targets? [y/N] " TARGET_CONFIRM
    case "$TARGET_CONFIRM" in
      y|Y|yes|YES)
        ;;
      *)
        echo "Aborted."
        exit 1
        ;;
    esac
  fi
}

run_stage confirm_targets always_redo \
  "$(sha256_file "$TARGETS_JSON")" probe_true stage_confirm_targets

# ---------------------------------------------------------------------------
# S4 — sandbox (bwrap) preflight (always_redo, pure probe)
# ---------------------------------------------------------------------------
stage_sandbox_probe() {
# Phase 4 bwrap-only migration: passwordless sudo to BURST_USER is no
# longer required; bursts run as the supervisor user inside bwrap. BURST_USER is
# retained as a parameter for Phase 5 mechanical removal.
if ! command -v bwrap >/dev/null 2>&1; then
  echo "ERROR: bwrap is required for sandboxed agent bursts." >&2
  exit 1
fi
# This probe answers one question — can bwrap run a trivial command as a burst
# would on this host — and it must answer it without touching anything real.
# `probe_sandbox` passes work_dir straight to `wrap_command`, whose first act is
# `repo = work_dir.resolve()` followed by creating every path in the worker's
# writable allowlist under it. Pointing that at the repo's PARENT (which is what
# this did while the repo already existed, i.e. on every --reset and --resume)
# scattered Tablet/, reference/, .trellis/ and .lake/build next to the repo —
# on this host, next to live runs. Pointing it at Path.cwd() when the repo did
# not exist yet did the same to whatever directory the operator ran setup from.
# A dedicated empty directory setup created and deletes is the only work_dir
# that is nobody else's.
if ! PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$SANDBOX_PROBE_DIR" "$BURST_HOME" <<'PY'
import sys
from pathlib import Path

from trellis.config import SandboxConfig
from trellis.sandbox import probe_sandbox

probe_dir = Path(sys.argv[1]).resolve()
burst_home = Path(sys.argv[2]).resolve()
ok, detail = probe_sandbox(
    sandbox=SandboxConfig(enabled=True, backend="bwrap"),
    work_dir=probe_dir,
    burst_home=burst_home,
)
if not ok:
    print(detail, file=sys.stderr)
    raise SystemExit(1)
PY
then
  echo "ERROR: bwrap exists but is not usable for sandboxed bursts on this host." >&2
  exit 1
fi
}

run_stage sandbox_probe always_redo "$(sha256_args "$BURST_HOME")" \
  probe_true stage_sandbox_probe

# Toolchain-clobber guard. `--reset` recreates the repo from the CURRENT
# defaults, and `restart_configured_run.sh` — the documented gold-standard
# restart path — runs exactly that. So on a machine that has pulled a
# toolchain bump, restarting a months-old run would silently migrate it: the
# repo is deleted below and `lean-toolchain` rewritten from
# $MATHLIB_TOOLCHAIN, moving a live formalization across Lean releases and
# thousands of mathlib commits. The operator experiences it as "I restarted
# the run and everything broke", with nothing naming the cause.
#
# A run's toolchain is a property OF THAT RUN, so an existing pin outranks
# our default. Refuse, name both values, and require an explicit
# --adopt-toolchain to migrate. Same shape as the challenge-spec
# disagreement check above: the more authoritative pin wins and the
# disagreement is loud.
#
# Checked BEFORE the reset wipes the evidence, and skipped when the operator
# asked for the migration.
if [ "$RESET" -eq 1 ] && [ "$ADOPT_TOOLCHAIN" -ne 1 ] && [ -f "$REPO/lean-toolchain" ]; then
  EXISTING_TOOLCHAIN="$(tr -d '[:space:]' < "$REPO/lean-toolchain")"
  WANTED_TOOLCHAIN="$(printf '%s' "$MATHLIB_TOOLCHAIN" | tr -d '[:space:]')"
  if [ -n "$EXISTING_TOOLCHAIN" ] && [ "$EXISTING_TOOLCHAIN" != "$WANTED_TOOLCHAIN" ]; then
    cat >&2 <<EOF
ERROR: --reset would change this run's Lean toolchain.

  existing ($REPO/lean-toolchain):
      $EXISTING_TOOLCHAIN
  setup default (MATHLIB_TOOLCHAIN):
      $WANTED_TOOLCHAIN

A run's toolchain belongs to that run. Recreating the repo from a newer
default silently migrates an in-progress formalization across Lean releases
and mathlib revisions, which can break proofs that were closed against the
old library and invalidates the mathlib prewarm.

  - to restart this run UNCHANGED, pin the toolchain it already has:
        MATHLIB_TOOLCHAIN=$EXISTING_TOOLCHAIN MATHLIB_REV=<its rev> $0 ...
    (its mathlib rev is recorded in $REPO/lake-manifest.json)
  - to migrate it deliberately, re-run with --adopt-toolchain
EOF
    exit 1
  fi
fi

if [ "$RESET" -eq 1 ]; then
  echo "  Resetting existing project artifacts..."
  [ -f "$SCRIPT_DIR/stop.sh" ] && "$SCRIPT_DIR/stop.sh" "$REPO" >/dev/null 2>&1 || true
  tmux_cmd kill-session -t "$PROJECT_SLUG" >/dev/null 2>&1 || true
  # Phase 4: repo is supervisor-owned; no sudo needed.
  rm -rf "$REPO"
  rm -rf "$PROJECT_STATIC_DIR"
  if [ -e "$REPO" ]; then
    echo "ERROR: failed to fully remove existing repo during --reset: $REPO" >&2
    exit 1
  fi
  # The ledger lived in the repo we just removed; a reset is a clean build.
  ledger_reset
fi

# ---------------------------------------------------------------------------
# S5 — repo skeleton, paper copy, rubrics, lakefile, toolchain (always_redo)
#
# Every write here is a whole-file overwrite of a deterministic function of the
# inputs, so re-running cannot leave a mixture of two builds.
# ---------------------------------------------------------------------------
stage_skeleton() {
mkdir -p "$REPO/paper" "$REPO/Tablet"
mkdir -p "$REPO/.trellis/logs" "$REPO/.trellis/scripts" "$REPO/.trellis/checkpoints"
mkdir -p "$REPO/.trellis/staging" "$REPO/.trellis/viewer/state-at" "$REPO/.trellis/chats" "$REPO/.trellis/scratch"
mkdir -p "$REPO/.trellis/runtime"

cp "$PAPER" "$REPO/paper/$PAPER_NAME"
echo "  Copied paper to $REPO/paper/$PAPER_NAME"

CHALLENGE_TARGETS_REL=""
if [ -n "$CHALLENGE_TARGETS" ]; then
  if [ ! -f "$CHALLENGE_TARGETS" ]; then
    echo "ERROR: --challenge-targets file not found: $CHALLENGE_TARGETS" >&2
    exit 1
  fi
  mkdir -p "$REPO/challenge"
  # The previous copy is chmod a-w, so it must be removed rather than
  # overwritten in place — otherwise a re-run (resume) fails on a
  # read-only destination.
  rm -f "$REPO/challenge/challenge_targets.json"
  cp "$CHALLENGE_TARGETS" "$REPO/challenge/challenge_targets.json"
  chmod a-w "$REPO/challenge/challenge_targets.json"
  CHALLENGE_TARGETS_REL="challenge/challenge_targets.json"
  echo "  Copied challenge targets to $REPO/challenge/challenge_targets.json"
fi

# One filespec, at the canonical name, holding the backend's spec — the same
# rule the canonical rubrics below follow. Every prompt fragment, on both
# backends, names the spec `FILESPEC.md` in its prose. Shipping the Lean spec
# under that name on an Isabelle run pointed workers at `Tablet/<Node>.lean`
# and the `-- BODY` marker while they authored `.thy` files.
FILESPEC_SRC="$SOURCE_ROOT/FILESPEC.md"
if [ "$DEFAULT_TARGET" = "isabelle_hol" ]; then
  FILESPEC_SRC="$SOURCE_ROOT/FILESPEC_isabelle.md"
fi
cp "$FILESPEC_SRC" "$REPO/FILESPEC.md"
echo "  Wrote FILESPEC.md (from $(basename "$FILESPEC_SRC"))"

# Materialize the four canonical verifier rubrics at the project root so the
# instruction in `TRELLIS_FORMALIZATION_SCHEME{,_verifier}.md` ("read
# FAITHFULNESS.md, SUBSTANTIVENESS.md, CORRESPONDENCE.md, SOUNDNESS.md at the
# project root") matches what the agents actually find. The kernel inlines the
# same content into every verifier prompt as well, so this is a redundant safety
# net for human readers and for agents who chose to verify the on-disk file.
# Without it the verifiers' `comments` fields fill up with "X.md was not present
# at the repository root" notes which then propagate into reviewer context and
# the next verifier's `previous_own_findings_by_lane`, polluting the substantive
# feedback channel.
# CORRESPONDENCE and SOUNDNESS have per-backend variants: the kernel inlines
# the `_isabelle` sibling into every prompt on an `isabelle_hol` tablet (the
# `isa_or_lean` hook in request_contracts.rs). Write the SAME variant to disk,
# or the rubric an agent reads at the project root contradicts the one it was
# handed in its prompt — Lean phrasing (`sorry`, `lake`, "closed in Lean") on
# an Isabelle tablet. The other three lanes are NL/`.tex` and port verbatim.
for canonical in CORRESPONDENCE DEVIATIONS FAITHFULNESS SOUNDNESS SUBSTANTIVENESS PROCESS_RULES; do
  src="$SOURCE_ROOT/trellis/prompt_fragments/canonical/${canonical}.md"
  if [ "$DEFAULT_TARGET" = "isabelle_hol" ] && \
     [ -f "$SOURCE_ROOT/trellis/prompt_fragments/canonical/${canonical}_isabelle.md" ]; then
    src="$SOURCE_ROOT/trellis/prompt_fragments/canonical/${canonical}_isabelle.md"
  fi
  cp "$src" "$REPO/${canonical}.md"
done
echo "  Wrote canonical verifier rubrics (CORRESPONDENCE.md, DEVIATIONS.md, FAITHFULNESS.md, SOUNDNESS.md, SUBSTANTIVENESS.md) + PROCESS_RULES.md"

if [ "$DEFAULT_TARGET" = "isabelle_hol" ]; then
  # Isabelle backend: the toolchain is the Isabelle session, not lake/mathlib,
  # so no lakefile / lean-toolchain. Seed the import-root preamble NODE
  # `Tablet/Preamble.thy` (the analogue of `Tablet/Preamble.lean`); the worker
  # imports `Tablet_Preamble` and the checker scaffold projects this to the
  # session dir. Theory name is the projected stem `Tablet_Preamble`.
  cat > "$REPO/Tablet/Preamble.thy" <<'PREAMBLE_THY'
theory Tablet_Preamble
  imports Main
begin

end
PREAMBLE_THY
  echo "  Wrote Tablet/Preamble.thy"
else
  cat > "$REPO/lakefile.lean" <<'LAKEFILE'
import Lake
open Lake DSL

package «tablet» where
  leanOptions := #[
    ⟨`autoImplicit, false⟩
  ]

@[default_target]
lean_lib «Tablet» where
  srcDir := "."

require mathlib from git
  "https://github.com/leanprover-community/mathlib4" @ "__MATHLIB_REV__"
LAKEFILE
  sed -i "s/__MATHLIB_REV__/$MATHLIB_REV/" "$REPO/lakefile.lean"
  echo "  Wrote lakefile.lean"

  echo "$MATHLIB_TOOLCHAIN" > "$REPO/lean-toolchain"
  echo "  Wrote lean-toolchain ($MATHLIB_TOOLCHAIN)"
  echo "  Pinned mathlib revision ($MATHLIB_REV)"

  cat > "$REPO/Tablet/Preamble.lean" <<'PREAMBLE'
-- Preamble: shared imports for all tablet nodes.
-- Add specific Mathlib imports here (never `import Mathlib`).
PREAMBLE
  echo "  Wrote Tablet/Preamble.lean"
fi

cat > "$REPO/APPROVED_AXIOMS.json" <<'AXIOMS'
{
  "global": [],
  "nodes": {}
}
AXIOMS
echo "  Wrote APPROVED_AXIOMS.json"

cat > "$REPO/HUMAN_INPUT.md" <<'HUMAN'
# Human Input

Write human guidance for the supervisor here when requested.
HUMAN

cat > "$REPO/INPUT_REQUEST.md" <<'REQUEST'
# Input Request

The supervisor will write explicit requests for human input here when needed.
REQUEST
}

CHALLENGE_TARGETS_REL=""
run_stage skeleton always_redo \
  "$(sha256_args "$(sha256_file "$PAPER")" "$PAPER_NAME" "$(sha256_file "$CHALLENGE_TARGETS")" "$MATHLIB_REV" "$MATHLIB_TOOLCHAIN")" \
  probe_true stage_skeleton

# ---------------------------------------------------------------------------
# S6 — config from template, git init, checker scripts, tablet support
#      (always_redo)
#
# The config is rendered from the template and the resolved targets, so it is a
# pure function of this invocation's inputs — including on a resume, which is
# why reference-paper registration (S7) re-checks that its config entry
# survived this rewrite.
# ---------------------------------------------------------------------------
stage_config_init() {
PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$REPO" "$CONFIG_TEMPLATE" "$CONFIG_OUT" "$POLICY_TEMPLATE" "$POLICY_OUT" "$PAPER_NAME" "$PROJECT_SLUG" "$TARGETS_JSON" "$BURST_USER" "$BURST_GROUP" "$BURST_HOME" "$LOOGLE_ENABLED_JSON" "$CHALLENGE_TARGETS_REL" <<'PY'
import json
import os
import shutil
import sys
from pathlib import Path

from trellis.checking import write_scripts
from trellis.config import load_config
from trellis.git_ops import init_repo
from trellis.project_paths import project_chats_dir, project_tmp_dir
from trellis.runtime.kernel_cli import run_kernel_cli
from trellis.worker_scratch import ensure_worker_scratch_workspace

repo = Path(sys.argv[1]).resolve()
config_template = Path(sys.argv[2]).resolve()
config_out = Path(sys.argv[3]).resolve()
policy_template = Path(sys.argv[4]).resolve()
policy_out = Path(sys.argv[5]).resolve()
paper_name = sys.argv[6]
slug = sys.argv[7]
resolved_targets_path = Path(sys.argv[8]).resolve()
burst_user = sys.argv[9]
burst_group = sys.argv[10]
burst_home = sys.argv[11]
loogle_enabled = sys.argv[12] == "true"
challenge_targets_rel = sys.argv[13] if len(sys.argv) > 13 else ""
state_dir = repo / ".trellis"
paper_path = repo / "paper" / paper_name


def write_atomic(path: Path, text: str) -> None:
    """tmp + rename in the same directory (add_reference_paper.sh discipline).

    A truncate-write of trellis.config.json is a real hazard: the operator's
    config, the viewer's project classification, and every later load_config
    all read this file, and a crash (or ENOSPC) part way through a plain write
    leaves unparseable JSON behind with no way to tell it from a config that
    was never written.
    """

    tmp_path = path.with_name(path.name + ".setup.tmp")
    tmp_path.write_text(text, encoding="utf-8")
    os.replace(tmp_path, path)


init_repo(repo)

parsed = json.loads(config_template.read_text(encoding="utf-8"))
if not isinstance(parsed, dict):
    raise SystemExit("Config template must be a JSON object")
data = parsed
data["repo_path"] = str(repo)
data["state_dir"] = ".trellis"
data["policy_path"] = "trellis.policy.json"

sandbox = data.setdefault("sandbox", {})
sandbox["enabled"] = True
sandbox["backend"] = "bwrap"

# Explicit per-project Loogle setting (required --loogle on|off at setup).
# When off, the worker prompt omits the Loogle helper fragment.
data["loogle"] = {"enabled": loogle_enabled}

tmux = data.setdefault("tmux", {})
tmux["session_name"] = slug
# burst_user is no longer required (post-bwrap-only); write it for
# backwards compatibility with stale loaders.
tmux["burst_user"] = burst_user
tmux["burst_group"] = burst_group
tmux["burst_home"] = burst_home

workflow = data.setdefault("workflow", {})
workflow["paper_tex_path"] = f"paper/{paper_name}"
workflow["approved_axioms_path"] = "APPROVED_AXIOMS.json"
workflow["human_input_path"] = "HUMAN_INPUT.md"
workflow["input_request_path"] = "INPUT_REQUEST.md"

resolved_targets = json.loads(resolved_targets_path.read_text(encoding="utf-8"))
if not isinstance(resolved_targets, dict):
    raise SystemExit("Resolved main-result targets must be a JSON object")
labels = resolved_targets.get("labels", [])
targets = resolved_targets.get("targets", [])
workflow["main_result_labels"] = labels
workflow["main_result_targets"] = targets
# The env set S2 resolved under, already normalized (or None for the default).
# Written as-is so load_config and any later add_paper_targets_action resolve
# under the identical set; the absent-key default stays absent, keeping a
# no-knob config byte-identical to a pre-knob setup.
resolved_envs = resolved_targets.get("main_result_envs")
if isinstance(resolved_envs, list) and resolved_envs:
    workflow["main_result_envs"] = resolved_envs
if challenge_targets_rel:
    workflow["challenge_targets_path"] = challenge_targets_rel

chat = data.setdefault("chat", {})
chat["root_dir"] = str(project_chats_dir(state_dir))
chat["repo_name"] = slug
chat["project_name"] = slug.replace("_", " ").title() + " Formalization"

git_cfg = data.setdefault("git", {})
git_cfg.setdefault("remote_url", None)
git_cfg.setdefault("remote_name", "origin")
git_cfg.setdefault("branch", "master")
git_cfg.setdefault("author_name", ".trellis")
git_cfg.setdefault("author_email", "trellis@localhost")

write_atomic(config_out, json.dumps(data, indent=2) + "\n")

if policy_template.exists():
    write_atomic(policy_out, policy_template.read_text(encoding="utf-8"))

config = load_config(config_out)
write_scripts(config.repo_path, config.state_dir)
# Tablet-support sync bootstrap (mirrors the kernel's InitFromConfig gate).
#
# Lean: `sync-tablet-support` runs LOCALLY in check.py's atomic_actions (no
# checker socket), rendering `Tablet/{INDEX,README,header}` + the `Tablet.lean`
# umbrella the supervisor workspace expects present from the start, so it runs
# at setup.
#
# Isabelle: the workspace render is socket-only — it dispatches the server-only
# `isabelle-sync-session`, which the checker services from its own socket
# runtime root. Setup runs BEFORE any checker server starts (and `Tablet/` holds
# only the Preamble), so the socket op has no server to reach and would fail.
# Defer it exactly like InitFromConfig does: the run loop's first
# `observe_nodes_parallel` hoists `sync_tablet_render_support_from_repo` once the
# checker is up. Skipping it here also keeps the Lean-only `Tablet.lean` umbrella
# off an Isabelle repo (the Isabelle scaffold is the socket-side `ROOT`).
targets_isabelle = (
    str((data.get("workflow") or {}).get("default_target") or "").strip()
    == "isabelle_hol"
)
if not targets_isabelle:
    sync_result = run_kernel_cli({"action": "sync_tablet_support", "repo_path": str(repo)})
    if sync_result.get("status") != "sync_tablet_support_ok":
        raise SystemExit(f"failed to sync tablet support artifacts: {sync_result}")

ensure_worker_scratch_workspace(repo, reset=True)
project_tmp_dir(state_dir).mkdir(parents=True, exist_ok=True)
PY
echo "  Initialized config, scripts, and support artifacts"
}

run_stage config_init always_redo \
  "$(sha256_args "$(sha256_file "$CONFIG_TEMPLATE")" "$(sha256_file "$POLICY_TEMPLATE")" \
     "$(sha256_file "$TARGETS_JSON")" "$PAPER_NAME" "$PROJECT_SLUG" "$BURST_USER" \
     "$BURST_GROUP" "$BURST_HOME" "$LOOGLE_ENABLED_JSON" "$CHALLENGE_TARGETS_REL")" \
  probe_true stage_config_init

# ---------------------------------------------------------------------------
# S7 — reference papers (always_redo, one ledger record per reference id)
#
# Register additional reference papers (repeatable --reference). One code
# path with the live-run flow: scripts/add_reference_paper.sh writes
# paper/refs/<id>.tex, appends the config entry, and commits both. The
# kernel init seeds workflow.reference_papers into the initial state.
#
# This stage is always_redo, not verify_skip: S6 above rewrites
# trellis.config.json from the template on every run, which drops the
# workflow.reference_papers entries a previous invocation appended, so a
# resume must always re-register to put them back. (The design filed S7 as
# verify-skip; that is only consistent with an S6 that preserves the entries,
# which would mean trusting existing config content instead of re-deriving it.
# Re-registering costs a transcode and a normalize — milliseconds — and
# reproduces byte-identical content, so add_reference_paper.sh's commit is a
# no-op.)
#
# What the ledger IS load-bearing for here is input pinning on top of
# add_reference_paper.sh's own guarantees. The script is sha-idempotent over
# the STORED bytes (identical content re-registers cleanly; different content
# is an immutability hard stop) — but it cannot see this repo's history, so
# before removing a stored file to re-register it we require the recorded
# output sha to prove this setup is the file's author AND the recorded
# inputs_sha to prove the source file and source_id are the ones the repo was
# set up with. A file with any other content — hand-edited, or a different
# source for the same id — stops the run.
# ---------------------------------------------------------------------------
reference_config_entry_present() {
  local ref_id="$1"
  python3 - "$CONFIG_OUT" "$ref_id" <<'PY'
import json
import sys

config_path, ref_id = sys.argv[1:3]
try:
    data = json.loads(open(config_path, encoding="utf-8").read())
except Exception:
    raise SystemExit(1)
entries = data.get("workflow", {}).get("reference_papers", [])
if not isinstance(entries, list):
    raise SystemExit(1)
for entry in entries:
    if isinstance(entry, dict) and entry.get("id") == ref_id:
        raise SystemExit(0)
raise SystemExit(1)
PY
}

stage_reference_papers() {
  local spec stage_id inputs_sha stored stored_sha
  for spec in "${REFERENCE_SPECS[@]:-}"; do
    [[ -z "$spec" ]] && continue
    parse_reference_spec "$spec" || exit 1
    stage_id="reference_paper:$REF_SPEC_ID"
    inputs_sha="$(sha256_args "$(sha256_file "$REF_SPEC_FILE")" "$REF_SPEC_SOURCE")"
    stored="$REPO/paper/refs/$REF_SPEC_ID.tex"

    if [ -e "$stored" ]; then
      stored_sha="$(sha256_file "$stored")"
      if ! ledger_is_done "$stage_id" "$inputs_sha" \
         || [ "${LEDGER_OUT[$stage_id]:-}" != "$stored_sha" ]; then
        echo "ERROR: $stored already exists and was not written by this setup from" >&2
        echo "       $REF_SPEC_FILE; reference papers are immutable — use a new id." >&2
        exit 1
      fi
      # Provably ours: remove it so the one code path that owns both the file
      # and the config entry can write them again.
      echo "  Re-registering reference paper $REF_SPEC_ID"
      rm -f "$stored"
    fi

    ledger_record "$stage_id" always_redo running "$inputs_sha"
    "$SCRIPT_DIR/add_reference_paper.sh" "$REPO" "$REF_SPEC_FILE" --id "$REF_SPEC_ID" --source-id "$REF_SPEC_SOURCE"
    ledger_record "$stage_id" always_redo done "$inputs_sha" "$(sha256_file "$stored")"
  done
}

stage_reference_papers

# A resume that omits a --reference spec an earlier invocation was given would
# otherwise silently ship a config registering no reference paper while the
# file sits in paper/refs/ — S6 regenerates the config from the template, and
# only specs named on THIS command line get re-appended. Stop loudly instead of
# quietly dropping grounding material from the run.
check_reference_registry_complete() {
  local path ref_id
  [ -d "$REPO/paper/refs" ] || return 0
  for path in "$REPO"/paper/refs/*.tex; do
    [ -e "$path" ] || continue
    ref_id="$(basename "$path" .tex)"
    if ! reference_config_entry_present "$ref_id"; then
      echo "ERROR: $path exists but workflow.reference_papers has no entry for '$ref_id'." >&2
      echo "       Re-run with the same --reference $ref_id=<file>[:<source_id>] spec this" >&2
      echo "       repo was set up with, or start over with --reset." >&2
      exit 1
    fi
  done
}

check_reference_registry_complete

# ---------------------------------------------------------------------------
# S8 — permissions walk + nested chats git (always_redo, idempotent)
# ---------------------------------------------------------------------------
stage_permissions_chats() {
python3 - "$REPO" "$BURST_GROUP" <<'PY'
import grp
import os
import sys
from pathlib import Path

repo = Path(sys.argv[1]).resolve()
group = sys.argv[2]
gid = grp.getgrnam(group).gr_gid

def chmod_dir(path: Path, mode: int) -> None:
    try:
        os.chown(str(path), -1, gid)
    except (PermissionError, OSError):
        pass
    try:
        os.chmod(str(path), mode)
    except (PermissionError, OSError):
        pass

def chmod_file(path: Path, mode: int) -> None:
    try:
        os.chown(str(path), -1, gid)
    except (PermissionError, OSError):
        pass
    try:
        os.chmod(str(path), mode)
    except (PermissionError, OSError):
        pass

skip_dirs = {'.git'}
for root, dirs, files in os.walk(repo):
    root_path = Path(root)
    dirs[:] = [d for d in dirs if d not in skip_dirs]
    chmod_dir(root_path, 0o2775)
    for name in files:
        path = root_path / name
        mode = 0o664
        if path.parent.name in {'scripts', 'bin'} or path.suffix == '.sh':
            mode = 0o775
        chmod_file(path, mode)
PY
echo "  Normalized working-tree permissions for shared use"

git -C "$REPO" config core.sharedRepository group

echo "  Initializing nested local chats git repo..."
git -C "$REPO/.trellis/chats" init >/dev/null 2>&1
git -C "$REPO/.trellis/chats" config user.name "trellis-chats" >/dev/null 2>&1
git -C "$REPO/.trellis/chats" config user.email "trellis-chats@localhost" >/dev/null 2>&1
cat > "$REPO/.trellis/chats/README.md" <<'CHATREADME'
# Local Chat History

This nested git repo stores project-local chat/session history.
It is intentionally outside the parent formalization repo history.
CHATREADME
git -C "$REPO/.trellis/chats" add README.md >/dev/null 2>&1
if [ -n "$(git -C "$REPO/.trellis/chats" status --porcelain)" ]; then
  git -C "$REPO/.trellis/chats" commit -m "Initialize local chat history repo" >/dev/null 2>&1
fi
}

run_stage permissions_chats always_redo "$(sha256_args "$BURST_GROUP")" \
  probe_true stage_permissions_chats

# ---------------------------------------------------------------------------
# S9 — Lean prewarm (verify_skip): lake update, mathlib seed or cache get,
#      lake build, smoke `lake env lean`. Minutes to tens of minutes.
#
# The probe is the artifact's own validity, never the ledger alone: Mathlib's
# top-level olean has to be there, and it has to be backed either by lake's own
# content-addressed traces (what an incremental clean build would trust) or by
# the tar-seed completion sentinel, which is renamed into place together with
# the seeded tree.
#
# Isabelle target: the Lean prewarm (lake/lean/mathlib) is meaningless on an
# Isabelle repo — there is no lakefile, the scratch file is `example.thy`, and
# the prebuilt HOL heap is already warm. Both functions branch on
# `$DEFAULT_TARGET` to the Tablet_Base session prewarm instead
# (`stage_prewarm_isabelle` below); the probe returns 1 there because
# `isabelle build` is itself incremental, so a resume just re-runs the (cheap
# when warm) stage rather than trusting a heap probe we don't have.
# ---------------------------------------------------------------------------
probe_prewarm() {
  if [ "$DEFAULT_TARGET" = "isabelle_hol" ]; then
    return 1
  fi
  local build_dir="$REPO/.lake/packages/mathlib/.lake/build"
  if [ ! -f "$build_dir/lib/lean/Mathlib.olean" ]; then
    return 1
  fi
  if [ -f "$build_dir/.trellis-mathlib-seed-complete" ]; then
    return 0
  fi
  if [ -n "$(find "$build_dir" -name '*.trace' -print -quit 2>/dev/null)" ]; then
    return 0
  fi
  return 1
}

stage_prewarm() {
if [ "$DEFAULT_TARGET" = "isabelle_hol" ]; then
  stage_prewarm_isabelle
  return
fi
echo "  Prewarming Lean dependencies and build artifacts as burst user..."
PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$REPO" "$BURST_GROUP" "$BURST_HOME" "$ELAN_HOME" "$BURST_PATH" "$MATHLIB_BUILD_TAR" <<'PY'
import sys
from pathlib import Path

from trellis.setup_ops import run_setup_prewarm

run_setup_prewarm(
    repo_path=Path(sys.argv[1]).resolve(),
    burst_group=sys.argv[2],
    burst_home=Path(sys.argv[3]).resolve(),
    elan_home=Path(sys.argv[4]).resolve(),
    burst_path=sys.argv[5],
    mathlib_build_tar=Path(sys.argv[6]).resolve() if sys.argv[6] else None,
)
PY
}

# Isabelle prewarm (Option B): prebuild the small warm `Tablet_Base` session
# so the worker + checker inherit HOL-Analysis+Probability+Complex_Main warm
# via the parent chain (no in-burst library builds). The heavy
# `HOL-Probability` DISTRIBUTION heap is a PREREQUISITE — prebuilt separately
# and once into the shared system heaps (the operator builds it ahead of time;
# we do NOT rebuild it here). This step builds only the tiny `Tablet_Base`
# image on top of that heap, thread-capped so it is courteous alongside a
# co-resident run. Dispatched from `stage_prewarm` on the isabelle_hol target.
stage_prewarm_isabelle() {
  echo "  Prewarming Isabelle Tablet_Base session (warm HOL-Probability parent)..."

  # Render the scaffold ROOT (the Tablet_Base + Tablet stanzas) into the
  # per-tablet session dir so `isabelle build -D` sees the `Tablet_Base` session.
  ISA_SESSION_DIR="$REPO/isabelle"
  PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$ISA_SESSION_DIR" <<'PY'
import sys
from pathlib import Path

from trellis.checker import isabelle_scaffold as scaffold

session_dir = Path(sys.argv[1]).resolve()
# Writes ROOT (both Tablet_Base + Tablet stanzas) + Tablet_Preamble.thy. The
# node set is whatever Tablet_<Node>.thy files already sit in the session dir
# (none yet at setup), which is fine: Tablet_Base needs no node theories.
summary = scaffold.sync_session(session_dir)
print(f"  Wrote scaffold ROOT: {summary['root_path']}")
PY

  # Resolve the Isabelle distribution root the canonical way (TRELLIS_ISABELLE_HOME
  # override → pinned install), matching the worker sandbox bind.
  ISABELLE_HOME="$(PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - <<'PY'
from trellis.host_runtime import worker_isabelle_home
print(worker_isabelle_home())
PY
)"

  # Build ONLY Tablet_Base into the SHARED system heaps, thread-capped. The
  # HOL-Probability parent is already a system heap (prebuilt once), so `-b`
  # here materializes just the small Tablet_Base image on top — it does NOT
  # rebuild HOL-Probability.
  "$ISABELLE_HOME/bin/isabelle" build \
    -b \
    -o system_heaps=true \
    -o threads=4 \
    -D "$ISA_SESSION_DIR" \
    Tablet_Base
  echo "  Built Tablet_Base session heap (system_heaps, threads=4)."
}

# The tarball is identified by path + size + mtime rather than by content: it
# is multi-gigabyte, and sha256-ing it on every run would cost minutes for a
# check that exists to save time. This detects the case that matters — the tar
# at a given path being replaced between runs — without the scan.
mathlib_build_tar_fingerprint() {
  if [ -n "$MATHLIB_BUILD_TAR" ] && [ -f "$MATHLIB_BUILD_TAR" ]; then
    stat -c '%n:%s:%Y' "$MATHLIB_BUILD_TAR"
  else
    echo "absent"
  fi
}

run_stage prewarm verify_skip \
  "$(sha256_args "$MATHLIB_REV" "$MATHLIB_TOOLCHAIN" "$(mathlib_build_tar_fingerprint)" "$BURST_PATH" "$BURST_HOME")" \
  probe_prewarm stage_prewarm

# ---------------------------------------------------------------------------
# S10 — provider CLI validation (always_redo, pure probes)
# ---------------------------------------------------------------------------
stage_provider_cli() {
CONFIGURED_PROVIDERS="$(PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$CONFIG_OUT" <<'PY'
from pathlib import Path
import sys

from trellis.config import load_config

config = load_config(Path(sys.argv[1]).resolve())
providers = {config.worker.provider, config.reviewer.provider}
if config.easy_worker is not None:
    providers.add(config.easy_worker.provider)
if config.hard_worker is not None:
    providers.add(config.hard_worker.provider)
for agent in config.verification.correspondence_agents:
    providers.add(agent.provider)
for agent in config.verification.soundness_agents:
    providers.add(agent.provider)
for name in sorted(p for p in providers if p):
    print(name)
PY
)"

echo "  Validating provider CLI access..."
# Phase 4 bwrap-only migration: providers run as the supervisor user inside bwrap;
# CLI auth comes from the supervisor's ~/.codex, ~/.claude, ~/.gemini.
#
# GATE H: validate under EXACTLY the PATH the real worker burst uses
# (`host_runtime.worker_path_env(burst_home)`), NOT $BURST_PATH. The burst
# launches as `env PATH=worker_path_env(...) bwrap ... <provider>`; if we
# validated under a different (richer) $BURST_PATH the check could print
# "codex: ok" while the burst still exits 127. `worker_path_env` resolves the
# provider CLIs the same way the sandbox read-only binds do, so this check
# now fails loudly when the burst won't find the CLI.
WORKER_BURST_PATH="$(PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$BURST_HOME" <<'PY'
import sys
from pathlib import Path
from trellis.host_runtime import worker_path_env
print(worker_path_env(Path(sys.argv[1]).resolve()))
PY
)"
if grep -qx 'codex' <<<"$CONFIGURED_PROVIDERS"; then
  env HOME="$BURST_HOME" ELAN_HOME="$ELAN_HOME" PATH="$WORKER_BURST_PATH" \
    bash -lc "set -euo pipefail; command -v codex >/dev/null || { echo \"codex not found on the worker sandbox PATH (\$PATH); install it where trellis can reach it (per-user npm-global, nvm, or /usr/local/bin — see INSTALLATION.md)\" >&2; exit 1; }; timeout 10s codex --version >/dev/null"
  echo "    codex: ok"
fi
if grep -qx 'claude' <<<"$CONFIGURED_PROVIDERS"; then
  env HOME="$BURST_HOME" ELAN_HOME="$ELAN_HOME" PATH="$WORKER_BURST_PATH" \
    bash -lc "set -euo pipefail; command -v claude >/dev/null || { echo \"claude not found on the worker sandbox PATH (\$PATH); install it where trellis can reach it (per-user npm-global, nvm, or /usr/local/bin — see INSTALLATION.md)\" >&2; exit 1; }; timeout 10s claude --version >/dev/null"
  echo "    claude: ok"
fi
if grep -qx 'gemini' <<<"$CONFIGURED_PROVIDERS"; then
  # GEMINI_CLI_TRUST_WORKSPACE=true: gemini's interactive trust dialog
  # cannot be answered headlessly. Without this env (or `--skip-trust`)
  # gemini exits 55 with "Gemini CLI is not running in a trusted
  # directory" — the same gate that bites tmux_backend's smoke path.
  # Production runs handle this via ensure_gemini_accessibility_settings;
  # this validation step needs the env equivalent.
  env HOME="$BURST_HOME" ELAN_HOME="$ELAN_HOME" PATH="$WORKER_BURST_PATH" \
    bash -lc "command -v gemini >/dev/null || { echo \"gemini not found on the worker sandbox PATH (\$PATH); install it where trellis can reach it (per-user npm-global, nvm, or /usr/local/bin — see INSTALLATION.md)\" >&2; exit 1; }"
  env HOME="$BURST_HOME" ELAN_HOME="$ELAN_HOME" PATH="$WORKER_BURST_PATH" \
    GEMINI_CLI_TRUST_WORKSPACE=true \
    python3 - <<'PY'
import os
import subprocess

env = os.environ.copy()
subprocess.run(
    [
        "gemini",
        "--approval-mode=yolo",
        "-p",
        "Reply with OK only.",
        "--output-format",
        "text",
    ],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.PIPE,
    timeout=45,
    check=True,
    env=env,
)
PY
  echo "    gemini: ok"
fi
}

run_stage provider_cli always_redo "$(sha256_args "$(sha256_file "$CONFIG_OUT")" "$BURST_HOME")" \
  probe_true stage_provider_cli

# ---------------------------------------------------------------------------
# S11 — worker-side shared access + sandboxed worker environment (always_redo)
#
# The `lake env lean` compile of the scratch `example.lean` proves the
# supervisor user can read/build the repo tree. This is the Lean toolchain; on
# an Isabelle repo there is no lake (and the scratch is `example.thy`), and the
# worker-side Isabelle toolchain is instead validated by the
# sandboxed-worker-environment probe below (`probe_worker_environment`, which
# checks `isabelle` + the prebuilt HOL heap on an isabelle target). Gate on the
# backend so an Isabelle setup runs no lake/lean here.
# ---------------------------------------------------------------------------
stage_worker_access() {
if [ "$DEFAULT_TARGET" != "isabelle_hol" ]; then
echo "  Validating worker-side shared access..."
# Phase 4: no sudo wrap; the supervisor user owns the repo and runs lake directly.
env \
  HOME="$BURST_HOME" \
  ELAN_HOME="$ELAN_HOME" \
  PATH="$BURST_PATH" \
  bash -lc "
    set -euo pipefail
    umask 0002
    git config --global --add safe.directory '$REPO' >/dev/null 2>&1 || true
    for package_dir in '$REPO'/.lake/packages/*; do
      if [ -e \"\$package_dir/.git\" ]; then
        git config --global --add safe.directory \"\$package_dir\" >/dev/null 2>&1 || true
      fi
    done
    cd '$REPO'
    lake env lean .trellis/scratch/example.lean
  "
fi

# The supervisor-side tablet acceptance check routes through the unified
# checker server (no host-lake fallback), so it requires a live
# TRELLIS_CHECKER_SOCKET. Setup runs before any checker server is started
# (the supported launcher, restart_configured_run.sh, starts the server
# *after* setup), so skip this inline check when no socket is exported.
# The build steps above already validated the Lean project; the real
# acceptance check runs at run time via the checker server.
if [ -n "${TRELLIS_CHECKER_SOCKET:-}" ] && [ -S "${TRELLIS_CHECKER_SOCKET}" ]; then
  echo "  Validating supervisor-side deterministic checks..."
  python3 "$REPO/.trellis/scripts/check.py" tablet "$REPO"
else
  echo "  Skipping supervisor-side tablet check (no checker server during setup; validated at first run)."
fi

echo "  Validating sandboxed worker environment..."
PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$REPO" "$BURST_HOME" "$CONFIG_OUT" <<'PY'
from pathlib import Path
import os
import sys

from trellis.config import SandboxConfig, load_config
from trellis.sandbox import probe_worker_environment

repo = Path(sys.argv[1]).resolve()
burst_home = Path(sys.argv[2]).resolve()
config = load_config(Path(sys.argv[3]).resolve())
providers = {
    str(config.worker.provider or "").strip(),
    str(config.reviewer.provider or "").strip(),
}
if config.easy_worker is not None:
    providers.add(str(config.easy_worker.provider or "").strip())
if config.hard_worker is not None:
    providers.add(str(config.hard_worker.provider or "").strip())
for agent in config.verification.correspondence_agents:
    providers.add(str(agent.provider or "").strip())
for agent in config.verification.soundness_agents:
    providers.add(str(agent.provider or "").strip())
providers.discard("")
# The checker-surface certification materializes tablet oleans through the
# acceptance path, which (no host-lake fallback) requires a live checker socket.
# Setup runs before any checker server starts, so certify only when a socket is
# exported; the bwrap/provider validation above runs either way.
_socket = os.environ.get("TRELLIS_CHECKER_SOCKET", "")
_certify = bool(_socket) and Path(_socket).is_socket()
ok, detail = probe_worker_environment(
    sandbox=SandboxConfig(enabled=True, backend="bwrap"),
    repo_path=repo,
    burst_home=burst_home,
    provider_commands=sorted(providers),
    certify_checker_surface=_certify,
)
if not ok:
    raise SystemExit(detail or "sandboxed worker environment probe failed")
if not _certify:
    print("  (skipped checker-surface certification: no checker server during setup; validated at first run)")
PY
}

run_stage worker_access always_redo "$(sha256_args "$REPO" "$BURST_PATH" "$BURST_HOME")" \
  probe_true stage_worker_access

# ---------------------------------------------------------------------------
# S12 — viewer route symlinks (always_redo, idempotent)
# ---------------------------------------------------------------------------
stage_viewer_routes() {
  mkdir -p "$PROJECT_STATIC_DIR"
  ln -sfn "$REPO/.trellis/viewer" "$PROJECT_STATIC_DIR/api"
  ln -sfn "$SOURCE_ROOT/viewer/public/index.html" "$PROJECT_STATIC_DIR/index.html"
  echo "  Linked project viewer route to repo-local viewer data"
}

run_stage viewer_routes always_redo "$(sha256_args "$PROJECT_STATIC_DIR" "$REPO")" \
  probe_true stage_viewer_routes

# ---------------------------------------------------------------------------
# S13 — lock sweep + initial commit (always_redo)
#
# A resumed setup legitimately has nothing left to commit, which is the only
# reason the commit was ever allowed to fail silently. Check for that case
# explicitly so a genuine commit failure (hooks, index lock, identity) stops
# setup instead of leaving an uncommitted tree that the first checkpoint
# `reset --hard` would throw away.
# ---------------------------------------------------------------------------
stage_initial_commit() {
  find "$REPO/.trellis" -name '*.lock' -delete 2>/dev/null || true
  git -C "$REPO" add -A
  if [ -z "$(git -C "$REPO" status --porcelain)" ]; then
    echo "  Nothing to commit (working tree already matches HEAD)"
  else
    # Backend-neutral wording (isabelle branch): the repo may be a Lean or an
    # Isabelle formalization project.
    git -C "$REPO" commit -m "Initial repo setup with paper and formalization project" >/dev/null
  fi
}

run_stage initial_commit always_redo "$(sha256_args "$REPO")" \
  probe_true stage_initial_commit

echo ""
echo "Setup complete."
echo "  Repo:          $REPO"
echo "  Config:        $CONFIG_OUT"
echo "  Viewer route:  /trellis/$PROJECT_SLUG/"
echo "  Verified with worker-side and supervisor-side tablet checks."
if [ "$LOOGLE_SETTING" = "off" ]; then
  echo ""
  echo "  Loogle is OFF (loogle.enabled=false): the worker prompt omits the Loogle"
  echo "  helper. The worker skill files still carry a 'Loogle First' section — edit"
  echo "  them to remove the Loogle guidance since no server is configured:"
  echo "    $SOURCE_ROOT/skills/THEOREM_STATING_WORKER.md"
  echo "    $SOURCE_ROOT/skills/PROOF_FORMALIZATION_WORKER.md"
fi
