#!/usr/bin/env bash
# Durable viewer job for prose program-verification genesis.
#
# User inputs are GOAL.md, a required target archive, and an optional supporting
# archive. The campaign repository, runtime, and trust authority are outputs only. In particular,
# this script never accepts an existing campaign path and never writes Lean;
# setup/extraction generate the model and bootstrap is the sole authority on
# whether that generated trusted base is admissible.
set -euo pipefail
umask 0077

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
PROJECTS_ROOT="${TRELLIS_PROJECTS_ROOT:-$HOME/math}"
if [[ -d "$PROJECTS_ROOT" ]]; then PROJECTS_ROOT="$(cd "$PROJECTS_ROOT" && pwd -P)"; fi
JOBS_ROOT="$PROJECTS_ROOT/.trellis-viewer/create-jobs"
PV_CONFIG_TEMPLATE="$ROOT_DIR/examples/trellis.pv.config.json"

TRELLIS_TMUX_SOCKET="${TRELLIS_TMUX_SOCKET:-trellis}"
HEARTBEAT_SECS="${TRELLIS_CREATE_HEARTBEAT_SECS:-10}"
STALE_SECS="${TRELLIS_CREATE_STALE_SECS:-60}"
VERIFY_SECS="${TRELLIS_CREATE_VERIFY_SECS:-60}"
UPLOAD_LIMIT_BYTES="${TRELLIS_CREATE_UPLOAD_LIMIT_BYTES:-20971520}"
SLUG_RE='^[A-Za-z][A-Za-z0-9_-]{0,63}$'

# The Rust language corpus is framework-wide bootstrap evidence, not a third
# operator input. setup_pv_repo.sh deliberately has fail-closed placeholders
# for ad-hoc invocations; the viewer path supplies the reviewed pins itself so
# its complete seed remains exactly GOAL.md + target + optional supporting files.
export RUST_REFERENCE_REV="${RUST_REFERENCE_REV:-86635e30bf861a038dc197d7e16fd09e7e514e7a}"
export NOMICON_REV="${NOMICON_REV:-5012a37c682b26c4e19433888ed2ca9b129696ca}"
export RUST_STD_REV="${RUST_STD_REV:-14210df0e27ccd7d9e6a05b8085cbd438e4bbc65}"

die() { echo "trellis-pv-create: $*" >&2; exit 2; }
source "$ROOT_DIR/scripts/lean_parallelism_env.sh"
tmux_cmd() { trellis_parallelism_tmux "$@"; }
# `grep -q` stops reading at the first match, so under `pipefail` a matching
# session that tmux lists before its last one made the pipeline fail on SIGPIPE
# and reported a live session as gone. Collect the names first.
session_alive() {
  local names
  names="$(tmux_cmd list-sessions -F '#S' 2>/dev/null || true)"
  grep -Fxq -- "$1" <<<"$names"
}

action="${1:-}"
slug="${2:-}"
shift 2 2>/dev/null || true
[[ -n "$action" && -n "$slug" ]] || die "usage: pv_create_run.sh <start|retry|stop|delete|run> <slug> [options]"
[[ "$slug" =~ $SLUG_RE ]] || die "invalid slug '$slug' (must match $SLUG_RE)"

JOB_DIR="$JOBS_ROOT/$slug"
JOB_JSON="$JOB_DIR/job.json"
STATUS_JSON="$JOB_DIR/status.json"
CREATE_LOG="$JOB_DIR/create.log"
PHASE_ENV="$JOB_DIR/phase_env.sh"
PHASE_LAUNCHER="$JOB_DIR/run_phase.sh"
REPO="$PROJECTS_ROOT/$slug"
RUNTIME_ROOT="$REPO-runtime"
AUTHORITY_ROOT="$JOB_DIR/authority"
GOAL_FORMAT="callable"
MARKER="$REPO/.trellis-creating"
SESSION="trellis-create-$slug"
RUN_SESSION="trellis-run-$slug"

json_field() {
  python3 -c '
import json, sys
try: value = json.load(open(sys.argv[1], encoding="utf-8"))
except Exception: raise SystemExit(0)
for key in sys.argv[2].split("."):
    if not isinstance(value, dict): raise SystemExit(0)
    value = value.get(key)
    if value is None: raise SystemExit(0)
print(json.dumps(value) if isinstance(value, (dict, list)) else value, end="")
' "$1" "$2" 2>/dev/null || true
}

select_goal_format() {
  GOAL_FORMAT="${1:-callable}"
  case "$GOAL_FORMAT" in
    callable) PV_CONFIG_TEMPLATE="$ROOT_DIR/examples/trellis.pv.config.json" ;;
    native)
      PV_CONFIG_TEMPLATE="$ROOT_DIR/examples/trellis.native.pv.config.json"
      AUTHORITY_ROOT="$JOB_DIR/native"
      local generation
      generation="$(json_field "$JOB_JSON" native_generation)"
      PREPARATION_ROOT="$AUTHORITY_ROOT/$slug-preparation"
      if [[ -n "$generation" && "$generation" != 0 ]]; then
        local suffix
        suffix="$(printf '%04d' "$generation")"
        RUN_SESSION="trellis-run-$slug-r$suffix"
        PREPARATION_ROOT="$AUTHORITY_ROOT/generations/$suffix/$slug-r$suffix-preparation"
        RUNTIME_ROOT="$(json_field "$JOB_JSON" runtime_root)"
      fi
      ;;
    *) die "goal format must be native or callable" ;;
  esac
}

write_status() {
  STATE="$1" STAGE="$2" ERROR="${3:-}" SLUG="$slug" PHASE="genesis" \
    python3 - "$STATUS_JSON" <<'PY'
import json, os, sys, time
from pathlib import Path
target = Path(sys.argv[1])
try:
    previous = json.loads(target.read_text(encoding="utf-8"))
except Exception:
    previous = {}
now = int(time.time())
record = {
    "slug": os.environ["SLUG"],
    "state": os.environ["STATE"],
    "stage": os.environ["STAGE"],
    "phase": os.environ["PHASE"],
    "started_ts": previous.get("started_ts") or now,
    "updated_ts": now,
    "pid": os.getppid(),
}
if os.environ.get("ERROR"):
    record["error"] = os.environ["ERROR"]
tmp = target.with_name(target.name + f".tmp.{os.getpid()}")
tmp.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
tmp.replace(target)
PY
}

HEARTBEAT_PID=""
start_heartbeat() {
  (
    while :; do
      sleep "$HEARTBEAT_SECS"
      state="$(json_field "$STATUS_JSON" state)"
      stage="$(json_field "$STATUS_JSON" stage)"
      [[ -n "$state" ]] && write_status "$state" "$stage" "$(json_field "$STATUS_JSON" error)"
    done
  ) &
  HEARTBEAT_PID=$!
}
stop_heartbeat() {
  if [[ -n "$HEARTBEAT_PID" ]]; then
    kill "$HEARTBEAT_PID" 2>/dev/null || true
    wait "$HEARTBEAT_PID" 2>/dev/null || true
    HEARTBEAT_PID=""
  fi
}
fail_phase() {
  stop_heartbeat
  ATTEMPT_FAILED=1
  write_status "$1" "$2" "$3"
  echo "trellis-pv-create: $3" >&2
  exit 1
}

write_phase_files() {
  python3 - "$PHASE_ENV" <<'PY'
import os, re, shlex, sys
from pathlib import Path
keep = re.compile(
    r"^(PATH|HOME|ELAN_HOME|RUSTUP_HOME|RUSTUP_TOOLCHAIN|CARGO_HOME|CARGO_TARGET_DIR|"
    r"PYTHONPATH|TRELLIS_PROJECTS_ROOT|TRELLIS_TMUX_SOCKET|TRELLIS_TRELLIS_KERNEL_CMD|TRELLIS_NATIVE_TOOLS|"
    r"TRELLIS_CREATE_[A-Z0-9_]+|TRELLIS_PV_CREATE_[A-Z0-9_]+|"
    r"TRELLIS_PROVIDERS_KEYS_FILE|TRELLIS_API_STATE_DIR|KIMI_BASE_URL|DEEPSEEK_BASE_URL|"
    r"TRELLIS_LEAN_PARALLELISM|TRELLIS_BURST_LEAN_PARALLELISM|LEAN_NUM_THREADS|"
    r"TRELLIS_CHECK_JOBS|TRELLIS_CHECK_JOBS_PARALLELISM|TRELLIS_CHECK_JOBS_FLOOR_GIB|TRELLIS_CHECK_JOBS_RESERVE_GIB|"
    r"TRELLIS_CHECK_READONLY_PARALLELISM|TRELLIS_CHECK_READONLY_RESERVE_GIB|TRELLIS_CHECKER_TELEMETRY_RESOURCES|"
    r"TRELLIS_CHECK_MEMORY_POLICY|TRELLIS_CHECK_MEMORY_POLICY_GENERATION|TRELLIS_CHECK_CGROUP|"
    r"TRELLIS_CHECK_COMPILER_SWAP_GIB|TRELLIS_CHECK_COMPILER_AGGREGATE_GIB|TRELLIS_CHECK_MAX_JOB_GIB|TRELLIS_CHECK_SERIAL_MAX_JOB_GIB|TRELLIS_CHECK_SERIAL_FLOOR_GIB|TRELLIS_CHECK_SERIAL_WAIT_SECONDS|"
    r"RUST_REFERENCE_[A-Z0-9_]+|NOMICON_[A-Z0-9_]+|RUST_STD_[A-Z0-9_]+|"
    r"PV_[A-Z0-9_]+|AENEAS_[A-Z0-9_]+|CHARON_[A-Z0-9_]+|OPAMROOT|OPAMSWITCH)$"
)
lines = ["# Generated by pv_create_run.sh; sourced by run_phase.sh.", ""]
# Explicit defaults prevent the tmux server's old memory policy leaking into
# a new phase. Keep these aligned with checker admission and restart launch.
environment = dict(os.environ)
for name, default in {
    "TRELLIS_CHECK_MEMORY_POLICY": "conservative",
    "TRELLIS_CHECK_MEMORY_POLICY_GENERATION": "0",
    "TRELLIS_CHECK_CGROUP": "off",
    "TRELLIS_CHECK_COMPILER_SWAP_GIB": "2",
    "TRELLIS_CHECK_COMPILER_AGGREGATE_GIB": "0",
    "TRELLIS_CHECK_MAX_JOB_GIB": "48",
    "TRELLIS_CHECK_SERIAL_MAX_JOB_GIB": "56",
    "TRELLIS_CHECK_SERIAL_FLOOR_GIB": "4",
    "TRELLIS_CHECK_SERIAL_WAIT_SECONDS": "300",
}.items():
    environment[name] = environment.get(name) or default
for name in sorted(environment):
    if keep.match(name) and name != "TRELLIS_CHECKER_SOCKET":
        lines.append(f"export {name}={shlex.quote(environment[name])}")
Path(sys.argv[1]).write_text("\n".join(lines) + "\n", encoding="utf-8")
PY
  python3 - "$PHASE_LAUNCHER" "$PHASE_ENV" "$SCRIPT_PATH" "$slug" "$CREATE_LOG" "${1:-run}" <<'PY'
import shlex, sys
from pathlib import Path
launcher, environment, script, slug, log, mode = map(str, sys.argv[1:])
body = f"""#!/usr/bin/env bash
set -uo pipefail
source {shlex.quote(environment)}
exec bash {shlex.quote(script)} {shlex.quote(mode)} {shlex.quote(slug)} >> {shlex.quote(log)} 2>&1
"""
Path(launcher).write_text(body, encoding="utf-8")
Path(launcher).chmod(0o700)
PY
}

spawn_job() {
  trellis_load_lean_parallelism "$JOB_JSON"
  write_phase_files "${1:-run}"
  tmux_cmd new-session -d -s "$SESSION" "bash '$PHASE_LAUNCHER'"
}

read_command() { # read_command ENV_NAME default...
  local variable="$1"
  shift
  COMMAND=()
  if [[ -n "${!variable:-}" ]]; then
    read -r -a COMMAND <<< "${!variable}"
  else
    COMMAND=("$@")
  fi
}

run_step() { # run_step state stage failed-state label command...
  local state="$1" stage="$2" failed="$3" label="$4"
  shift 4
  write_status "$state" "$stage"
  echo "trellis-pv-create: [$stage] $label"
  if ! "$@"; then
    fail_phase "$failed" "$stage" "$label failed — see create.log"
  fi
}

validate_settings() {
  local spec role value
  if [[ -n "$LEAN_PARALLELISM_ARG" ]]; then
    [[ "$LEAN_PARALLELISM_ARG" =~ ^[1-9][0-9]{0,15}$ ]] && [ "$LEAN_PARALLELISM_ARG" -le 9007199254740991 ] \
      || die "--lean-parallelism must be a positive integer"
  fi
  for spec in ${ROLE_PROVIDER_SPECS[@]+"${ROLE_PROVIDER_SPECS[@]}"}; do
    [[ "$spec" =~ ^[A-Za-z][A-Za-z0-9_.]*=(codex|claude|gemini|antigravity|deepseek|kimi)$ ]] || die "invalid --role-provider"
  done
  for spec in ${ROLE_BASE_URL_SPECS[@]+"${ROLE_BASE_URL_SPECS[@]}"}; do
    [[ "$spec" =~ ^[A-Za-z][A-Za-z0-9_.]*=https://[^[:space:]@?#]+$ ]] || die "invalid --role-base-url"
  done
  for spec in ${ROLE_MODEL_SPECS[@]+"${ROLE_MODEL_SPECS[@]}"}; do
    [[ "$spec" == *=* ]] || die "--role-model expects <config-role>=<model> (got: $spec)"
    role="${spec%%=*}"; value="${spec#*=}"
    [[ "$role" =~ ^[A-Za-z][A-Za-z0-9_.]*$ ]] \
      || die "--role-model role must match [A-Za-z][A-Za-z0-9_.]* (got: $role)"
    [[ "$value" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] \
      || die "--role-model model must match [A-Za-z0-9][A-Za-z0-9._-]* (got: $value)"
  done
  for spec in ${ROLE_EFFORT_SPECS[@]+"${ROLE_EFFORT_SPECS[@]}"}; do
    [[ "$spec" == *=* ]] || die "--role-effort expects <config-role>=<effort> (got: $spec)"
    role="${spec%%=*}"; value="${spec#*=}"
    [[ "$role" =~ ^[A-Za-z][A-Za-z0-9_.]*$ ]] \
      || die "--role-effort role must match [A-Za-z][A-Za-z0-9_.]* (got: $role)"
    [[ "$value" =~ ^[a-z][a-z0-9_-]*$ ]] \
      || die "--role-effort effort must match [a-z][a-z0-9_-]* (got: $value)"
  done
  if [[ -n "$REMOTE_URL_ARG" ]]; then
    [[ "$REMOTE_URL_ARG" =~ ^(git@[A-Za-z0-9._-]+:[A-Za-z0-9._/-]+(\.git)?|https://[A-Za-z0-9._-]+/[A-Za-z0-9._/-]+(\.git)?)$ ]] \
      || die "--remote-url must be git@host:owner/repo(.git) or https://host/owner/repo(.git) (got: $REMOTE_URL_ARG)"
  fi
  [[ "$ALLOW_SAME_MODEL_LANES_ARG" == "on" || "$ALLOW_SAME_MODEL_LANES_ARG" == "off" ]] \
    || die "--allow-same-model-lanes must be on or off"
}

role_overrides_json() {
  python3 - "$PV_CONFIG_TEMPLATE" ${ROLE_MODEL_SPECS[@]+"${ROLE_MODEL_SPECS[@]}"} -- \
    ${ROLE_EFFORT_SPECS[@]+"${ROLE_EFFORT_SPECS[@]}"} -- \
    ${ROLE_PROVIDER_SPECS[@]+"${ROLE_PROVIDER_SPECS[@]}"} -- \
    ${ROLE_BASE_URL_SPECS[@]+"${ROLE_BASE_URL_SPECS[@]}"} <<'PY'
import json, sys
split = sys.argv.index("--")
config = json.load(open(sys.argv[1], encoding="utf-8"))
roles = {}
def defaults(value, path=""):
    if isinstance(value, dict):
        if "provider" in value and "model" in value:
            roles.setdefault(path, {}).update({
                "provider": str(value.get("provider") or ""),
                "model": str(value.get("model") or ""),
                "effort": str(value.get("effort") or ""),
            })
        for key, child in list(value.items()):
            defaults(child, f"{path}.{key}" if path else key)
    elif isinstance(value, list):
        for child in value:
            defaults(child, path)
defaults(config)
second = sys.argv.index("--", split + 1)
third = sys.argv.index("--", second + 1)
for field, specs in (("model", sys.argv[2:split]), ("effort", sys.argv[split+1:second]), ("provider", sys.argv[second+1:third]), ("base_url", sys.argv[third+1:])):
    for spec in specs:
        role, value = spec.split("=", 1)
        if field == "provider" and roles.get(role, {}).get("provider") != value:
            for name, specs in (("model", sys.argv[2:split]), ("effort", sys.argv[split+1:second])):
                if not any(spec.startswith(role + "=") for spec in specs):
                    roles.setdefault(role, {}).pop(name, None)
        roles.setdefault(role, {})[field] = value
print(json.dumps(roles, separators=(",", ":")))
PY
}

# Merge the job's settings onto the explicit PV template. Phase 0 reads this
# same file before setup, and setup uses it as the source for the generated
# campaign config, so genesis and the in-run dispatcher cannot drift.
derive_job_template() {
  local out="$JOB_DIR/config-template.json"
  J_T_ROOT="$ROOT_DIR" \
  J_T_ROLE_OVERRIDES="$(json_field "$JOB_JSON" role_overrides)" \
  J_T_REMOTE="$(json_field "$JOB_JSON" remote_url)" \
  J_T_ALLOW_SAME="$(json_field "$JOB_JSON" allow_same_model_lanes)" \
  python3 - "$PV_CONFIG_TEMPLATE" "$out" <<'PY'
import json, os, sys
from pathlib import Path

data = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
sys.path.insert(0, os.environ["J_T_ROOT"])
from trellis.provider_overrides import apply_binding
selected_endpoints = {}
role_overrides = json.loads(os.environ.get("J_T_ROLE_OVERRIDES", "") or "{}")
if not isinstance(role_overrides, dict):
    raise SystemExit("job role_overrides must be an object")
applied = set()

def apply_roles(value, path=""):
    if isinstance(value, dict):
        selected = role_overrides.get(path)
        if "provider" in value and "model" in value and isinstance(selected, dict):
            apply_binding(value, selected, data, endpoints=selected_endpoints)
            applied.add(path)
        for key, child in list(value.items()):
            apply_roles(child, f"{path}.{key}" if path else key)
    elif isinstance(value, list):
        for child in value:
            apply_roles(child, path)

apply_roles(data)
unknown = sorted(set(role_overrides) - applied)
if unknown:
    raise SystemExit("job role_overrides contains unknown PV role(s): " + ", ".join(unknown))

phase0 = data.get("phase0")
if not isinstance(phase0, dict):
    raise SystemExit("PV config template lacks phase0 settings")
allow = os.environ.get("J_T_ALLOW_SAME", "false").strip().lower()
if allow not in {"true", "false"}:
    raise SystemExit("job allow_same_model_lanes must be boolean")
phase0["allow_same_model_lanes"] = allow == "true"
if "preparation" in data:
    phase0["allow_same_model_lanes"] = True
    data["system_feedback_halt"] = True

remote = os.environ.get("J_T_REMOTE", "").strip()
if remote:
    data.setdefault("git", {})["remote_url"] = remote

Path(sys.argv[2]).write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
PY
  printf '%s\n' "$out"
}

settings_acknowledgment() {
  python3 - "$JOB_JSON" <<'PY'
import json, sys
job = json.load(open(sys.argv[1], encoding="utf-8"))
roles = ", ".join(
    f"{role}={selected.get('model', '(template)')}/{selected.get('effort', '(template)')}"
    for role, selected in (job.get("role_overrides") or {}).items()
)
allow = "true" if job.get("allow_same_model_lanes") else "false"
print((roles + ("; " if roles else "")) + f"phase0.allow_same_model_lanes={allow}")
PY
}

enable_system_feedback_halt() {
  python3 - "$REPO/trellis.config.json" <<'PY'
import json, os, sys
from pathlib import Path

path = Path(sys.argv[1])
try:
    config = json.loads(path.read_text(encoding="utf-8"))
except Exception as exc:
    raise SystemExit(f"could not read generated campaign config: {exc}")
if not isinstance(config, dict):
    raise SystemExit("generated campaign config must be a JSON object")
config["system_feedback_halt"] = True
temporary = path.with_name(path.name + f".tmp.{os.getpid()}")
temporary.write_text(
    json.dumps(config, indent=2, sort_keys=True) + "\n", encoding="utf-8"
)
temporary.replace(path)
if json.loads(path.read_text(encoding="utf-8")).get("system_feedback_halt") is not True:
    raise SystemExit("generated campaign config did not retain system_feedback_halt=true")
PY
}

run_native_genesis() {
  trap stop_heartbeat EXIT
  start_heartbeat
  local config_template mode="${1:-run}"
  if [[ "$mode" == "run" ]]; then
    config_template="$(derive_job_template)" || fail_phase resolve_failed config "invalid native launch settings"
  fi
  read_command TRELLIS_PV_CREATE_NATIVE_CMD python3 "$ROOT_DIR/scripts/pv_native_create.py"
  run_step resolving preparation resolve_failed "preparing source and launching native modeling" \
    "${COMMAND[@]}" "$mode" "$JOB_DIR"
  select_goal_format native
  write_status verifying verify
  local waited=0 reported_wait=0
  # Native startup can spend minutes preparing checker dependencies before
  # the first dispatch. Keep the job visible as verifying while its runtime
  # session lives; elapsed time alone is not evidence of a failed launch.
  while :; do
    if "${COMMAND[@]}" started "$JOB_DIR"; then
      if session_alive "$RUN_SESSION"; then
        break
      fi
    elif [[ "$?" -eq 1 ]]; then
      fail_phase launch_failed verify "native runtime failed before its first dispatch — see create.log"
    fi
    if ! session_alive "$RUN_SESSION"; then
      fail_phase launch_failed verify "native runtime exited before its first dispatch — see create.log"
    fi
    if [[ "$waited" -ge "$VERIFY_SECS" && "$reported_wait" -eq 0 ]]; then
      echo "trellis-pv-create: native runtime is still initializing; waiting for its first dispatch"
      reported_wait=1
    fi
    sleep 2
    waited=$((waited + 2))
  done
  stop_heartbeat
  write_status done done
  echo "trellis-pv-create: native modeling is initialized and running"
}

run_genesis() {
  if [[ "$GOAL_FORMAT" == "native" ]]; then
    run_native_genesis
    return
  fi
  local attempt=""
  cleanup_attempt() {
    if [[ -n "$attempt" && -d "$attempt" ]]; then
      # A failed job must not lose the only record of what its agents did:
      # keep the burst logs and transcripts (small) beside the job before
      # the private attempt tree (source copies, replays) is removed.
      if [[ "${ATTEMPT_FAILED:-0}" == "1" ]]; then
        local salvage="$JOB_DIR/failed-attempt-logs"
        mkdir -p "$salvage"
        [[ -d "$attempt/agent-output/logs" ]] && cp -R -- "$attempt/agent-output/logs" "$salvage/agent-logs" 2>/dev/null
        [[ -d "$attempt/.trellis/chats/live" ]] && cp -RL -- "$attempt/.trellis/chats/live" "$salvage/transcripts" 2>/dev/null
        echo "trellis-pv-create: agent logs from the failed attempt kept at $salvage"
      fi
      rm -rf -- "$attempt"
    fi
  }
  trap 'stop_heartbeat; cleanup_attempt' EXIT
  start_heartbeat
  [[ -f "$JOB_DIR/GOAL.md" && -f "$JOB_DIR/target.tar.gz" ]] \
    || fail_phase resolve_failed intake "job inputs are incomplete"
  [[ ! -e "$REPO" && ! -e "$RUNTIME_ROOT" && ! -e "$AUTHORITY_ROOT" ]] \
    || fail_phase build_failed freshness "fresh-only construction refused: a campaign, runtime, or authority output already exists; delete this failed job and start with a new slug"
  local config_template
  config_template="$(derive_job_template)" \
    || fail_phase resolve_failed config "could not derive the requested PV config template"

  attempt="$JOB_DIR/attempt.$(date -u +%Y%m%dT%H%M%SZ).$$"
  mkdir "$attempt" \
    || fail_phase resolve_failed freshness "could not claim a fresh private attempt directory"
  local unpacked_target="$attempt/unpacked-target" unpacked_support="$attempt/unpacked-support"
  local unadapted="$attempt/unadapted" replayed="$attempt/replayed-adapted"
  local tree_manifest="$attempt/SOURCE_TREE_MANIFEST.json"
  local partition_manifest="$attempt/SOURCE_PARTITION_MANIFEST.json"
  mkdir "$attempt/work" \
    || fail_phase resolve_failed freshness "could not create fresh Phase-0 work directories"

  read_command TRELLIS_PV_CREATE_ARCHIVE_CMD python3 "$ROOT_DIR/scripts/pv_archive.py"
  run_step resolving unpack-target resolve_failed "validating and unpacking target archive" \
    "${COMMAND[@]}" extract "$JOB_DIR/target.tar.gz" "$unpacked_target" --limit "$UPLOAD_LIMIT_BYTES"
  local target_rel target_root support_rel support_root
  target_rel="$(python3 "$ROOT_DIR/scripts/pv_archive.py" inspect "$JOB_DIR/target.tar.gz" --limit "$UPLOAD_LIMIT_BYTES" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin).get("source_root", ""))')" \
    || fail_phase resolve_failed unpack-target "could not locate the uploaded target root"
  target_root="$unpacked_target${target_rel:+/$target_rel}"
  support_root=""
  if [[ -f "$JOB_DIR/support.tar.gz" ]]; then
    run_step resolving unpack-support resolve_failed "validating and unpacking supporting archive" \
      "${COMMAND[@]}" extract "$JOB_DIR/support.tar.gz" "$unpacked_support" --limit "$UPLOAD_LIMIT_BYTES"
    support_rel="$(python3 "$ROOT_DIR/scripts/pv_archive.py" inspect "$JOB_DIR/support.tar.gz" --limit "$UPLOAD_LIMIT_BYTES" \
      | python3 -c 'import json,sys; print(json.load(sys.stdin).get("source_root", ""))')" \
      || fail_phase resolve_failed unpack-support "could not locate the uploaded supporting root"
    support_root="$unpacked_support${support_rel:+/$support_rel}"
  fi

  read_command TRELLIS_PV_CREATE_ADAPT_CMD python3 "$ROOT_DIR/scripts/adapt_pv_source.py"
  local adapt_args=(--target-tree "$target_root" --out "$unadapted"
    --tree-manifest-out "$tree_manifest" --partition-manifest-out "$partition_manifest")
  [[ -n "$support_root" ]] && adapt_args+=(--supporting-tree "$support_root")
  run_step resolving partition resolve_failed "forming the target/supporting union and recording its partition" \
    "${COMMAND[@]}" "${adapt_args[@]}"
  [[ -f "$tree_manifest" && -f "$partition_manifest" ]] \
    || fail_phase resolve_failed partition "source union lacks its canonical manifests"

  mkdir "$AUTHORITY_ROOT" \
    || fail_phase build_failed freshness "could not claim a fresh trust-authority directory"
  mkdir "$AUTHORITY_ROOT/build" "$AUTHORITY_ROOT/plan" "$AUTHORITY_ROOT/work" \
    || fail_phase build_failed freshness "could not create fresh trust-authority work directories"
  local trust_cli="$AUTHORITY_ROOT/build/kernel/release/trellis_trust_cli"
  local filespec_checker="$AUTHORITY_ROOT/build/kernel/release/examples/scan_tablet_filespec"
  local prescribed_region_checker="$AUTHORITY_ROOT/build/kernel/release/examples/print_prescribed_region"
  run_step resolving trust-kernel build_failed "building the fresh release trust kernel" \
    env CARGO_TARGET_DIR="$AUTHORITY_ROOT/build/kernel" \
      cargo build --locked --release --manifest-path "$ROOT_DIR/kernel/Cargo.toml" \
        --bin trellis_trust_cli \
        --example scan_tablet_filespec \
        --example print_prescribed_region
  [[ -x "$trust_cli" ]] || fail_phase build_failed trust-kernel "trust kernel is not executable: $trust_cli"
  [[ -x "$filespec_checker" ]] || fail_phase build_failed trust-kernel "FILESPEC checker is not executable: $filespec_checker"
  [[ -x "$prescribed_region_checker" ]] || fail_phase build_failed trust-kernel "prescribed-region checker is not executable: $prescribed_region_checker"

  local phase0_root="$AUTHORITY_ROOT/phase0" phase0_output="$attempt/phase0.output.json"
  local charon_bin="${CHARON_BIN:-${TRELLIS_ROOT:-/path/to/trellis}/pv_spike/tools/charon/bin/charon}"
  local aeneas_bin="${AENEAS_BIN:-${TRELLIS_ROOT:-/path/to/trellis}/pv_spike/tools/aeneas/bin/aeneas}"
  local opam_root="${OPAMROOT:-$HOME/.opam}" opam_switch="${OPAMSWITCH:-5.3.0}"
  read_command TRELLIS_PV_CREATE_PHASE0_CMD python3 "$ROOT_DIR/scripts/pv_phase0.py"
  local phase0_archive_args=(--target-archive "$JOB_DIR/target.tar.gz")
  [[ -f "$JOB_DIR/support.tar.gz" ]] && phase0_archive_args+=(--supporting-archive "$JOB_DIR/support.tar.gz")
  run_step resolving phase0 resolve_failed "adapting and sealing the uploaded source generation" \
    "${COMMAND[@]}" --run "${phase0_archive_args[@]}" \
      --partition-manifest "$partition_manifest" \
      --unadapted-tree "$unadapted" --goal "$JOB_DIR/GOAL.md" \
      --attempt-root "$attempt" --authority-root "$phase0_root" \
      --kernel-cli "$trust_cli" --controller "$SCRIPT_PATH" \
      --filespec-checker "$filespec_checker" \
      --prescribed-region-checker "$prescribed_region_checker" \
      --charon-bin "$charon_bin" --aeneas-bin "$aeneas_bin" \
      --opam-root "$opam_root" --opam-switch "$opam_switch" \
      --lane-config "$config_template" \
      --result-out "$phase0_output"
  local phase0_genesis sealed_generation phase0_ledger goal_targets candidate_tree
  local recorded_tree_manifest recorded_partition_manifest
  phase0_genesis="$(json_field "$phase0_output" genesis_path)"
  sealed_generation="$(json_field "$phase0_output" sealed_generation_path)"
  phase0_ledger="$(json_field "$phase0_output" ledger_path)"
  goal_targets="$(json_field "$phase0_output" goal_targets_path)"
  candidate_tree="$(json_field "$phase0_output" candidate_tree_path)"
  recorded_tree_manifest="$(json_field "$phase0_output" source_tree_manifest_path)"
  recorded_partition_manifest="$(json_field "$phase0_output" source_partition_manifest_path)"
  [[ -f "$phase0_genesis" && -f "$sealed_generation" && -f "$phase0_ledger" \
      && -f "$goal_targets" && -f "$recorded_tree_manifest" \
      && -f "$recorded_partition_manifest" && -d "$candidate_tree" ]] \
    || fail_phase resolve_failed phase0 "Phase 0 reported sealed without its complete frozen handoff"
  cmp -s -- "$tree_manifest" "$recorded_tree_manifest" \
    || fail_phase resolve_failed phase0 "Phase 0 recorded a different union tree manifest"
  cmp -s -- "$partition_manifest" "$recorded_partition_manifest" \
    || fail_phase resolve_failed phase0 "Phase 0 recorded a different partition manifest"

  read_command TRELLIS_PV_CREATE_REPLAY_CMD "$trust_cli" replay-phase0-adaptation
  run_step resolving replay resolve_failed "replaying the sealed ledger from the frozen unadapted tree" \
    "${COMMAND[@]}" "$unadapted" "$phase0_ledger" "$goal_targets" \
      "$candidate_tree" "$replayed"

  read_command TRELLIS_PV_CREATE_SETUP_CMD bash "$ROOT_DIR/scripts/setup_pv_repo.sh"
  run_step building setup build_failed "building a fresh prose campaign" \
    "${COMMAND[@]}" --crate "$replayed" --unadapted-crate "$unadapted" --out-repo "$REPO" \
      --runtime-root "$RUNTIME_ROOT" --project-slug "$slug" \
      --goal "$JOB_DIR/GOAL.md" --aeneas-require-mode git \
      --charon-bin "$charon_bin" --aeneas-bin "$aeneas_bin" \
      --opam-switch "$opam_switch" \
      --filespec-checker "$filespec_checker" \
      --prescribed-region-checker "$prescribed_region_checker" \
      --cargo-target-dir "$AUTHORITY_ROOT/build/extraction" \
      --work-root "$AUTHORITY_ROOT/work/extraction" --defer-runtime-init \
      --config-template "$config_template" \
      --phase0-genesis "$phase0_genesis" --sealed-generation "$sealed_generation"
  enable_system_feedback_halt \
    || fail_phase build_failed setup "could not record system_feedback_halt=true in the generated campaign config"
  echo "trellis-pv-create: [setup] recorded campaign property system_feedback_halt=true in $REPO/trellis.config.json"
  touch "$MARKER" \
    || fail_phase build_failed setup "fresh campaign was built but its create-job marker could not be written"

  local phase0_production="$REPO/.trellis/phase0/PRODUCTION_RESULT.json"
  read_command TRELLIS_PV_CREATE_PHASE0_VERIFY_CMD "$trust_cli" verify-phase0-production
  run_step building production-binding build_failed "binding production extraction to the sealed generation" \
    "${COMMAND[@]}" "$REPO" "$phase0_genesis" "$sealed_generation" "$phase0_production"

  local builder="$ROOT_DIR/scripts/build_pv_trust_plan.py"
  local plan="$AUTHORITY_ROOT/plan/TRUST_PLAN.json"
  read_command TRELLIS_PV_CREATE_PLAN_CMD python3 "$builder"
  run_step building plan build_failed "recording GOAL resolution for every target" \
    "${COMMAND[@]}" --prose-targets "$REPO/pv_tablet.json" \
      --sealed-generation "$sealed_generation" \
      --goal-binding-report "$REPO/.trellis/phase0/GOAL_BINDING_REPORT.json" \
      --out "$plan"

  local trust_root="$AUTHORITY_ROOT/bootstrap" bootstrap_output="$attempt/bootstrap.output"
  write_status building bootstrap
  echo "trellis-pv-create: [bootstrap] asking the kernel to approve the generated trusted base"
  read_command TRELLIS_PV_CREATE_BOOTSTRAP_CMD "$trust_cli" bootstrap-campaign
  set +e
  "${COMMAND[@]}" "$REPO" "$plan" "$trust_cli" "$trust_root" \
    "$slug" "$slug-prose-genesis" "$phase0_genesis" "$sealed_generation" \
    2>&1 | tee "$bootstrap_output"
  local bootstrap_rc=${PIPESTATUS[0]}
  set -e
  if [[ "$bootstrap_rc" -ne 0 ]]; then
    local kernel_message
    kernel_message="$(tail -n 12 "$bootstrap_output")"
    fail_phase build_failed bootstrap "$kernel_message"
  fi

  local trust_config="$trust_root/trellis.config.trust-v1.json"
  local runtime_cli="$trust_root/evidence-root/bin/trellis-runtime-kernel"
  [[ -f "$trust_config" && -x "$runtime_cli" ]] \
    || fail_phase build_failed bootstrap "bootstrap completed without its trust config or runtime kernel"

  read_command TRELLIS_PV_CREATE_INIT_CMD bash "$ROOT_DIR/scripts/trellis.sh" init
  run_step initing init init_failed "initializing exactly once from the trust-bearing config" \
    env TRELLIS_TRELLIS_KERNEL_CMD="$runtime_cli" "${COMMAND[@]}" "$trust_config" "$RUNTIME_ROOT"

  read_command TRELLIS_PV_CREATE_GATE_CMD python3 "$ROOT_DIR/scripts/_setup_pv_repo_gates.py"
  run_step initing gates init_failed "running the post-initialization acceptance gates" \
    env TRELLIS_TRELLIS_KERNEL_CMD="$runtime_cli" PYTHONPATH="$ROOT_DIR${PYTHONPATH:+:$PYTHONPATH}" \
      "${COMMAND[@]}" "$RUNTIME_ROOT" "$trust_config" "$REPO"

  write_status launching launch
  local launch_attempt
  launch_attempt="$(PYTHONPATH="$ROOT_DIR${PYTHONPATH:+:$PYTHONPATH}" python3 -m trellis.launch_verification arm "$RUNTIME_ROOT" "$REPO" --runtime-cli "$runtime_cli")"
  local runner="${TRELLIS_PV_CREATE_RUNNER:-$ROOT_DIR/scripts/run_trust_v1_campaign.sh}"
  [[ -x "$runner" || -f "$runner" ]] || fail_phase launch_failed launch "campaign runner not found: $runner"
  if ! tmux_cmd new-session -d -s "$RUN_SESSION" \
      "cd '$ROOT_DIR' && env -u TRELLIS_SYSTEM_FEEDBACK_HALT TRELLIS_LAUNCH_ATTEMPT='$launch_attempt' TRELLIS_TMUX_SOCKET='$TRELLIS_TMUX_SOCKET' bash '$runner' '$RUNTIME_ROOT' '$REPO' '$trust_root' '$trust_cli' '$runtime_cli'"; then
    fail_phase launch_failed launch "could not start the campaign runner tmux session"
  fi

  write_status verifying verify
  local ready=0 outcome=done
  for ((i = 0; i < VERIFY_SECS; i += 2)); do
    if ! session_alive "$RUN_SESSION" && PYTHONPATH="$ROOT_DIR${PYTHONPATH:+:$PYTHONPATH}" python3 -m trellis.launch_verification paused "$RUNTIME_ROOT" "$REPO" --runtime-cli "$runtime_cli"; then
      ready=1; outcome=paused; break
    fi
    if [[ -e "$RUNTIME_ROOT/runtime_error_halt.json" || -e "$RUNTIME_ROOT/checker_disagreement_halt.json" || -e "$RUNTIME_ROOT/system_feedback_halt.json" ]]; then break; fi
    if [[ -f "$RUNTIME_ROOT/protocol_state.json" ]] && session_alive "$RUN_SESSION"; then
      ready=1
      break
    fi
    sleep 2
  done
  [[ "$ready" -eq 1 ]] \
    || fail_phase launch_failed verify "campaign runner did not remain live with protocol_state.json within ${VERIFY_SECS}s"

  rm -f "$MARKER"
  cleanup_attempt
  stop_heartbeat
  write_status done "$outcome"
  echo "trellis-pv-create: done — $slug is initialized, launched, and kernel-verified"
}

require_job() { [[ -d "$JOB_DIR" && -f "$JOB_JSON" ]] || die "no create job for '$slug'"; }

case "$action" in
  start)
    GOAL_ARG=""
    TARGET_ARG=""
    SUPPORT_ARG=""
    GOAL_NAME="GOAL.md"
    TARGET_NAME="target.tar.gz"
    SUPPORT_NAME="support.tar.gz"
    ROLE_MODEL_SPECS=()
    ROLE_EFFORT_SPECS=()
ROLE_PROVIDER_SPECS=()
ROLE_BASE_URL_SPECS=()
    LEAN_PARALLELISM_ARG=""
    REMOTE_URL_ARG=""
    ALLOW_SAME_MODEL_LANES_ARG="off"
    while [[ $# -gt 0 ]]; do
      case "$1" in
        --goal-format) GOAL_FORMAT="${2:?--goal-format requires native or callable}"; shift 2 ;;
        --goal) GOAL_ARG="${2:?--goal requires a path}"; shift 2 ;;
        --target) TARGET_ARG="${2:?--target requires a path}"; shift 2 ;;
        --support) SUPPORT_ARG="${2:?--support requires a path}"; shift 2 ;;
        --goal-name) GOAL_NAME="${2:?--goal-name requires a value}"; shift 2 ;;
        --target-name) TARGET_NAME="${2:?--target-name requires a value}"; shift 2 ;;
        --support-name) SUPPORT_NAME="${2:?--support-name requires a value}"; shift 2 ;;
        --role-model) ROLE_MODEL_SPECS+=("${2:?--role-model requires role=model}"); shift 2 ;;
        --role-effort) ROLE_EFFORT_SPECS+=("${2:?--role-effort requires role=effort}"); shift 2 ;;
      --role-provider) ROLE_PROVIDER_SPECS+=("${2:?--role-provider requires role=provider}"); shift 2 ;;
      --role-base-url) ROLE_BASE_URL_SPECS+=("${2:?--role-base-url requires role=url}"); shift 2 ;;
        --lean-parallelism) LEAN_PARALLELISM_ARG="${2:?--lean-parallelism requires a positive integer}"; shift 2 ;;
        --remote-url) REMOTE_URL_ARG="${2:?--remote-url requires a URL}"; shift 2 ;;
        --allow-same-model-lanes) ALLOW_SAME_MODEL_LANES_ARG="${2:?--allow-same-model-lanes requires on or off}"; shift 2 ;;
        *) die "unknown option: $1" ;;
      esac
    done
    select_goal_format "$GOAL_FORMAT"
    [[ -f "$GOAL_ARG" ]] || die "GOAL upload not found: $GOAL_ARG"
    GOAL_STORAGE="GOAL.md"
    if [[ "$GOAL_FORMAT" == "native" ]]; then
      GOAL_STORAGE="GOAL.json"
      NATIVE_TOOLS="${TRELLIS_NATIVE_TOOLS:-$PROJECTS_ROOT/.trellis-viewer/native-tools.json}"
      [[ -f "$NATIVE_TOOLS" ]] || die "native modeling tools have not been configured for this viewer"
      python3 "$ROOT_DIR/scripts/pv_native_create.py" preflight "$GOAL_ARG" "$NATIVE_TOOLS" >/dev/null \
        || die "native modeling input or operator tools are invalid"
    fi
    [[ -f "$TARGET_ARG" ]] || die "target archive upload not found: $TARGET_ARG"
    [[ -z "$SUPPORT_ARG" || -f "$SUPPORT_ARG" ]] \
      || die "supporting archive upload not found: $SUPPORT_ARG"
    [[ -f "$PV_CONFIG_TEMPLATE" ]] || die "PV config template not found: $PV_CONFIG_TEMPLATE"
    validate_settings
    # Model diversity is optional in native preparation, including CLI launches.
    [[ "$GOAL_FORMAT" == "native" ]] && ALLOW_SAME_MODEL_LANES_ARG="on"
    [[ ! -e "$REPO" && ! -e "$RUNTIME_ROOT" ]] \
      || die "a campaign or runtime already exists for '$slug'; choose a fresh slug"
    mkdir -p "$JOBS_ROOT"
    mkdir "$JOB_DIR" 2>/dev/null || die "a create job for '$slug' already exists at $JOB_DIR"
    cp "$GOAL_ARG" "$JOB_DIR/$GOAL_STORAGE"
    [[ "$GOAL_FORMAT" == "native" ]] && cp "$NATIVE_TOOLS" "$JOB_DIR/native-tools.json"
    cp "$TARGET_ARG" "$JOB_DIR/target.tar.gz"
    [[ -n "$SUPPORT_ARG" ]] && cp "$SUPPORT_ARG" "$JOB_DIR/support.tar.gz"
    J_SLUG="$slug" J_REPO="$REPO" J_RUNTIME="$RUNTIME_ROOT" J_AUTH="$AUTHORITY_ROOT" \
      J_GOAL_NAME="$GOAL_NAME" J_GOAL_FORMAT="$GOAL_FORMAT" J_GOAL_STORAGE="$GOAL_STORAGE" J_TARGET_NAME="$TARGET_NAME" \
      J_SUPPORT_NAME="$SUPPORT_NAME" J_HAS_SUPPORT="$([[ -n "$SUPPORT_ARG" ]] && echo 1 || echo 0)" \
      J_ROLE_OVERRIDES="$(role_overrides_json)" J_REMOTE_URL="$REMOTE_URL_ARG" \
      J_LEAN_PARALLELISM="$LEAN_PARALLELISM_ARG" \
      J_ALLOW_SAME="$ALLOW_SAME_MODEL_LANES_ARG" J_TEMPLATE="$PV_CONFIG_TEMPLATE" \
      python3 - "$JOB_JSON" <<'PY'
import json, os, sys, time
from pathlib import Path
record = {
    "slug": os.environ["J_SLUG"],
    "created_ts": int(time.time()),
    "create_flow": "prose-program-verification",
    "retry_policy": "fresh-only",
    "goal": os.environ["J_GOAL_STORAGE"],
    "goal_format": os.environ["J_GOAL_FORMAT"],
    "goal_name": os.environ["J_GOAL_NAME"],
    "target": "target.tar.gz",
    "target_name": os.environ["J_TARGET_NAME"],
    "repo": os.environ["J_REPO"],
    "runtime_root": os.environ["J_RUNTIME"],
    "authority_root": os.environ["J_AUTH"],
    "system_feedback_halt": True,
    "template": os.environ["J_TEMPLATE"],
    "role_overrides": json.loads(os.environ["J_ROLE_OVERRIDES"]),
    "lean_parallelism": int(os.environ["J_LEAN_PARALLELISM"]) if os.environ["J_LEAN_PARALLELISM"] else None,
    "remote_url": os.environ["J_REMOTE_URL"] or None,
    "allow_same_model_lanes": os.environ["J_ALLOW_SAME"] == "on",
}
if os.environ["J_HAS_SUPPORT"] == "1":
    record["supporting"] = "support.tar.gz"
    record["supporting_name"] = os.environ["J_SUPPORT_NAME"]
Path(sys.argv[1]).write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY
    derive_job_template >/dev/null \
      || die "could not derive the requested PV config template"
    write_status intake intake
    if ! spawn_job; then
      write_status resolve_failed phase-launch "could not start the background genesis tmux session"
      die "could not start the background genesis tmux session"
    fi
    echo "trellis-pv-create: started — job $JOB_DIR in tmux session $SESSION; campaign property system_feedback_halt=true"
    echo "trellis-pv-create: launch settings — $(settings_acknowledgment)"
    ;;
  run)
    require_job
    trellis_load_lean_parallelism "$JOB_JSON"
    select_goal_format "$(json_field "$JOB_JSON" goal_format)"
    run_genesis
    ;;
  reopen)
    require_job
    select_goal_format "$(json_field "$JOB_JSON" goal_format)"
    [[ "$GOAL_FORMAT" == "native" ]] || die "only native jobs support audited reopening"
    session_alive "$SESSION" && die "a preparation controller is already running for '$slug'"
    spawn_job reopen-run || die "could not start the preparation reopening controller"
    ;;
  reopen-run)
    require_job
    select_goal_format "$(json_field "$JOB_JSON" goal_format)"
    [[ "$GOAL_FORMAT" == "native" ]] || die "only native jobs support audited reopening"
    run_native_genesis reopen
    ;;
  resume-run)
    require_job
    select_goal_format "$(json_field "$JOB_JSON" goal_format)"
    [[ "$GOAL_FORMAT" == "native" ]] || die "only native preparation supports resume"
    run_native_genesis resume
    ;;
  retry)
    require_job
    select_goal_format "$(json_field "$JOB_JSON" goal_format)"
    session_alive "$SESSION" && die "a phase is already running for '$slug'"
    [[ "$(json_field "$JOB_JSON" create_flow)" == "prose-program-verification" ]] \
      || die "job '$slug' belongs to another create flow"
    [[ "$(json_field "$STATUS_JSON" state)" != done ]] || die "'$slug' is done; use the run controls"
    if ! session_alive "$RUN_SESSION" && PYTHONPATH="$ROOT_DIR${PYTHONPATH:+:$PYTHONPATH}" python3 -m trellis.launch_verification paused "$RUNTIME_ROOT" "$REPO"; then
      rm -f "$MARKER"
      write_status done paused
      exit 0
    fi
    retry_mode=run
    if [[ "$GOAL_FORMAT" == "native" ]] && { [[ -f "$PREPARATION_ROOT/.trellis/modeling/state.json" ]] || [[ "$(json_field "$JOB_JSON" reopening.pending)" == "True" ]]; }; then
      session_alive "$RUN_SESSION" && die "native campaign is already running"
      read_command TRELLIS_PV_CREATE_NATIVE_CMD python3 "$ROOT_DIR/scripts/pv_native_create.py"
      "${COMMAND[@]}" resume-check "$JOB_DIR" || die "native preparation is not ready to resume"
      retry_mode=resume-run
    else
      [[ ! -e "$REPO" && ! -e "$RUNTIME_ROOT" && ! -e "$AUTHORITY_ROOT" ]] \
        || die "fresh-only retry refused: delete this failed job and start with a new slug"
    fi
    write_status intake intake
    if ! spawn_job "$retry_mode"; then
      write_status resolve_failed phase-launch "could not restart the background genesis tmux session"
      die "could not restart the background genesis tmux session"
    fi
    if [[ "$retry_mode" == "resume-run" ]]; then
      echo "trellis-pv-create: resuming preserved preparation with its original goals and settings"
    else
      echo "trellis-pv-create: retrying from untouched uploads"
    fi
    ;;
  stop)
    require_job
    select_goal_format "$(json_field "$JOB_JSON" goal_format)"
    [[ $# -eq 0 ]] || die "unknown option: $1"
    [[ "$(json_field "$STATUS_JSON" state)" != "done" ]] \
      || die "'$slug' completed; stop refuses to interrupt a live campaign"
    if [[ "$GOAL_FORMAT" == "native" ]]; then
      session_alive "$RUN_SESSION" && die "native campaign is already running; use the run controls to pause it"
      read_command TRELLIS_PV_CREATE_NATIVE_CMD python3 "$ROOT_DIR/scripts/pv_native_create.py"
      "${COMMAND[@]}" stop "$JOB_DIR" || die "could not stop this job's preparation driver"
    fi
    stopped=()
    if session_alive "$SESSION"; then
      tmux_cmd kill-session -t "$SESSION" \
        || die "could not stop create tmux session '$SESSION'"
      stopped+=("$SESSION")
    fi
    # Phase-0 lane sessions are named by the bridge, one per worker/reviewer
    # lane. Each name tmux itself reports is killed by that exact name; no
    # pattern is handed to a killer that could match this process.
    while read -r phase_session; do
      [[ -n "$phase_session" ]] || continue
      tmux_cmd kill-session -t "$phase_session" \
        || die "could not stop Phase-0 tmux session '$phase_session'"
      stopped+=("$phase_session")
    done < <(if [[ "$GOAL_FORMAT" != "native" ]]; then tmux_cmd list-sessions -F '#S' 2>/dev/null | grep -E '^phase0-' || true; fi)
    for _ in $(seq 1 50); do
      session_alive "$SESSION" || break
      sleep 0.1
    done
    if session_alive "$SESSION"; then
      die "create tmux session '$SESSION' remained alive after kill-session"
    fi
    write_status resolve_failed stopped "stopped by operator"
    echo "trellis-pv-create: stopped ${#stopped[@]} tmux session(s) for '$slug': ${stopped[*]:-none}"
    ;;
  delete)
    require_job
    select_goal_format "$(json_field "$JOB_JSON" goal_format)"
    session_alive "$SESSION" && die "a phase is still running for '$slug'"
    [[ "$(json_field "$STATUS_JSON" state)" != "done" ]] \
      || die "'$slug' completed; delete refuses to remove a live campaign"
    confirms=()
    while [[ $# -gt 0 ]]; do
      case "$1" in
        --confirm) confirms+=("${2:?--confirm requires a path}"); shift 2 ;;
        *) die "unknown option: $1" ;;
      esac
    done
    doomed=("$JOB_DIR")
    [[ -e "$REPO" ]] && doomed+=("$REPO")
    [[ -e "$RUNTIME_ROOT" ]] && doomed+=("$RUNTIME_ROOT")
    if [[ ${#confirms[@]} -ne ${#doomed[@]} ]]; then
      DOOMED_JSON="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "${doomed[@]}")" \
        python3 -c 'import json,os; print(json.dumps({"error":"confirmation_required","paths":json.loads(os.environ["DOOMED_JSON"])}))'
      exit 3
    fi
    for ((i = 0; i < ${#doomed[@]}; i++)); do
      [[ "${confirms[$i]}" == "${doomed[$i]}" ]] || die "delete confirmation paths did not match"
    done
    if [[ "$GOAL_FORMAT" == "native" ]]; then
      session_alive "$RUN_SESSION" && die "native campaign is running; refusing to delete it as a failed creation"
      read_command TRELLIS_PV_CREATE_NATIVE_CMD python3 "$ROOT_DIR/scripts/pv_native_create.py"
      "${COMMAND[@]}" stop "$JOB_DIR" || die "could not stop native preparation before deletion"
    fi
    if session_alive "$RUN_SESSION"; then
      echo "trellis-pv-create: stopping campaign runner tmux session $RUN_SESSION before delete"
      tmux_cmd kill-session -t "$RUN_SESSION" \
        || die "could not stop campaign runner tmux session '$RUN_SESSION'"
      session_alive "$RUN_SESSION" \
        && die "campaign runner tmux session '$RUN_SESSION' remained alive; refusing delete"
    fi
    [[ -e "$RUNTIME_ROOT" ]] && rm -rf -- "$RUNTIME_ROOT"
    [[ -e "$REPO" ]] && rm -rf -- "$REPO"
    rm -rf -- "$JOB_DIR"
    echo "trellis-pv-create: deleted failed job outputs"
    ;;
  *) die "unknown action: $action" ;;
esac
