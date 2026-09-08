#!/usr/bin/env bash
# Create a trellis run as a durable, resumable, two-phase background job.
#
# This is the create-job orchestrator behind the viewer's run-creation wizard
# (VIEWER_RUN_CREATION_DESIGN.md §3.4, §4). It follows the trellis_pause.sh
# precedent: THE SCRIPT owns the state machine and the on-disk formats; the
# viewer is a thin caller, and a hand operator drives the same states with the
# same commands and sees identical progress on the landing page.
#
# THE TWO-PHASE PROCESS MODEL (design §4). A create job is not one long-lived
# process. Phase A (`intake` -> `resolving`) writes targets_resolution.json,
# parks the job at `awaiting_targets`, and EXITS. Phase B (`building` ->
# `initing` -> `launching` -> `verifying` -> `done`) is started by an explicit
# `confirm`. The park between them is process-DOWN on purpose: awaiting_targets
# is where users sit for hours or abandon entirely, and a pause held open by a
# live process is only as durable as that process — on-disk state with the
# process down cannot decay (trellis_pause.sh's own principle). Host reboot
# during the wait needs no recovery at all.
#
# WHERE STATE LIVES (design §3.1). Everything is under the job dir
#   <projects_root>/.trellis-viewer/create-jobs/<slug>/
#     job.json                 the request: paper, loogle, template, env map,
#                              main-result envs, references, and (after
#                              confirm) the selected candidate keys
#     status.json              {slug, state, stage, phase, started_ts,
#                              updated_ts, error?, pid?} — atomic tmp+rename;
#                              updated_ts is heartbeat-refreshed every
#                              $TRELLIS_CREATE_HEARTBEAT_SECS while a phase
#                              runs, so `updated_ts` staler than
#                              $TRELLIS_CREATE_STALE_SECS with the tmux
#                              session absent means `interrupted`
#     targets_resolution.json  phase A's output (design §5.1), rendered
#                              verbatim by the target page
#     targets_resolution.prev.json  the previous resolution, kept on every
#                              re-resolve so the viewer can diff the candidate
#                              set ("what changed?") instead of re-rendering
#     targets_selection.json   the confirmed selection translated into the
#                              kernel's raw_targets wire shape (design §5.6)
#     create.log               append-only stdout+stderr of every phase;
#                              rotated between phases at
#                              $TRELLIS_CREATE_LOG_ROTATE_BYTES (10 MB), the
#                              previous log kept at create.log.1 (row 28)
#     phase_env.sh, run_phase.sh   the recorded phase environment + launcher
#                              (see ENVIRONMENT below)
# plus the repo marker <repo>/.trellis-creating — phase B's first repo write,
# removed as the last act of a verified launch. The viewer classifies
# marker-bearing repos as creating/create_failed instead of as runs.
#
# The `mkdir` of the job dir is the atomic slug claim (design §7 rows 4-5).
#
# ENVIRONMENT. Phases run inside tmux session `trellis-create-<slug>` on the
# `-L $TRELLIS_TMUX_SOCKET` socket (default: trellis). tmux panes inherit the
# tmux SERVER's environment, not the caller's — so every spawn snapshots the
# caller's relevant environment into phase_env.sh and the pane sources it.
# That is also what makes the script testable: the stub commands below arrive
# through the snapshot exactly as the caller exported them.
#
# Stubbable commands (the TRELLIS_TRELLIS_KERNEL_CMD trick, plan §9):
#   TRELLIS_CREATE_RESOLVE_CMD   default: python3 scripts/resolve_paper_targets.py
#   TRELLIS_CREATE_SETUP_CMD     default: bash scripts/setup_repo.sh
#   TRELLIS_CREATE_INIT_CMD      default: bash scripts/trellis.sh init
#   TRELLIS_CREATE_CHECKER_CMD   default: scripts/trellis_checker_server.sh
#   TRELLIS_CREATE_RUN_CMD       default: scripts/trellis.sh run (in tmux)
#   TRELLIS_CREATE_PAUSE_CMD     default: bash scripts/trellis_pause.sh
# Tunables: TRELLIS_CREATE_HEARTBEAT_SECS (10), TRELLIS_CREATE_STALE_SECS (60),
# TRELLIS_CREATE_SOCKET_WAIT_SECS (30), TRELLIS_CREATE_VERIFY_SECS (60),
# TRELLIS_CREATE_LOG_ROTATE_BYTES (10485760), TRELLIS_CREATE_DISK_WARN_GB (10).
#
# Usage:
#   trellis_create_run.sh start   <slug> --paper <tex> --loogle on|off
#                                 [--template <config.json>]
#                                 [--role-model CONFIG_ROLE=MODEL]...
#                                 [--role-effort CONFIG_ROLE=EFFORT]...
#                                 [--env-map ALIAS=CANONICAL]...
#                                 [--main-result-envs LIST]
#                                 [--reference <id>=<file>[:<source_id>]]...
#   trellis_create_run.sh resolve <slug> [--paper <tex>]
#                                 [--env-map ALIAS=CANONICAL]...
#                                 [--main-result-envs LIST]
#                                 [--reference <id>=<file>[:<source_id>]]...
#                                 [--clear-references]
#   trellis_create_run.sh confirm <slug> --select <candidate-key> [--select ...]
#   trellis_create_run.sh retry   <slug>       (alias: resume)
#   trellis_create_run.sh status  <slug>       # JSON to stdout
#   trellis_create_run.sh delete  <slug> [--confirm <path>]...
#
# `retry` relaunches the CURRENT phase — resumability makes that cheap
# (setup's stage ledger skips verified stages; design §3). It is never
# `--reset`; the only wipe is the explicit `delete`, which refuses unless the
# caller names every doomed path (design §4).
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

TRELLIS_TMUX_SOCKET="${TRELLIS_TMUX_SOCKET:-trellis}"
export TRELLIS_TMUX_SOCKET
tmux_cmd() { tmux -L "$TRELLIS_TMUX_SOCKET" "$@"; }

PROJECTS_ROOT="${TRELLIS_PROJECTS_ROOT:-$HOME/math}"
# Canonicalize the projects root ONCE, here, before anything derives a path
# from it. Every repo path, runtime root, marker and — critically — the
# runtime root baked into the supervisor's own command line descends from
# this variable, so resolving it here makes them all canonical by
# construction instead of by convention.
#
# Why this is load-bearing rather than tidiness: `~/math` is a symlink to
# `/mnt/2ndSSD/math` on this host, so one directory has two spellings. The
# viewer resolves a project's runtime root through `fs.realpathSync`
# (`runtimeRootForProject`), while `trellis_pause.sh` finds the supervisor by
# matching that root LITERALLY against process command lines
# (`pgrep -f "bash .*trellis\.sh run $runtime_root"`). Launch under one
# spelling, resolve under the other, and the pgrep finds nothing — a healthy
# run reports `supervisor down` and classifies as `abandoned`. Observed
# exactly that on the goldberg_seymour run of 2026-08-27.
#
# `cd … && pwd -P` rather than `realpath`/`readlink -f`: it is POSIX shell,
# needs no coreutils version, and resolves every symlink component. A
# not-yet-existing root is left alone — `start` creates it and the next
# invocation canonicalizes.
# `die` is not defined until below, so this reports for itself.
if [[ -d "$PROJECTS_ROOT" ]]; then
  if ! PROJECTS_ROOT="$(cd "$PROJECTS_ROOT" && pwd -P)"; then
    echo "trellis-create: cannot resolve TRELLIS_PROJECTS_ROOT: $PROJECTS_ROOT" >&2
    exit 2
  fi
fi
JOBS_ROOT="$PROJECTS_ROOT/.trellis-viewer/create-jobs"

HEARTBEAT_SECS="${TRELLIS_CREATE_HEARTBEAT_SECS:-10}"
STALE_SECS="${TRELLIS_CREATE_STALE_SECS:-60}"
SOCKET_WAIT_SECS="${TRELLIS_CREATE_SOCKET_WAIT_SECS:-30}"
VERIFY_SECS="${TRELLIS_CREATE_VERIFY_SECS:-60}"
LOG_ROTATE_BYTES="${TRELLIS_CREATE_LOG_ROTATE_BYTES:-10485760}"
DISK_WARN_GB="${TRELLIS_CREATE_DISK_WARN_GB:-10}"

die() { echo "trellis-create: $*" >&2; exit 2; }

# The viewer's new-slug rule (plan §1.5): strict, no dots — `..` must be
# unrepresentable, and the slug becomes a repo path, tmux session names and a
# URL segment.
SLUG_RE='^[A-Za-z][A-Za-z0-9_-]{0,63}$'

action="${1:-}"
slug="${2:-}"
shift 2 2>/dev/null || true
[[ -n "$action" && -n "$slug" ]] \
  || die "usage: trellis_create_run.sh <start|resolve|confirm|retry|resume|status|delete|phase-a|phase-b> <slug> [opts]"
[[ "$slug" =~ $SLUG_RE ]] || die "invalid slug '$slug' (must match $SLUG_RE)"

JOB_DIR="$JOBS_ROOT/$slug"
JOB_JSON="$JOB_DIR/job.json"
STATUS_JSON="$JOB_DIR/status.json"
RESOLUTION_JSON="$JOB_DIR/targets_resolution.json"
PREV_RESOLUTION_JSON="$JOB_DIR/targets_resolution.prev.json"
SELECTION_JSON="$JOB_DIR/targets_selection.json"
CREATE_LOG="$JOB_DIR/create.log"
PHASE_ENV="$JOB_DIR/phase_env.sh"
PHASE_LAUNCHER="$JOB_DIR/run_phase.sh"
REPO="$PROJECTS_ROOT/$slug"
RUNTIME_ROOT="$REPO-runtime"
MARKER="$REPO/.trellis-creating"
SESSION="trellis-create-$slug"
CHECKER_SESSION="trellis-checker-$slug"
RUN_SESSION="trellis-run-$slug"
SIDECAR_SESSION="trellis-sidecar-$slug"
CHECKER_SOCKET="$RUNTIME_ROOT/sockets/checker.sock"

# ---------------------------------------------------------------------------
# Small JSON helpers. All writes are tmp+rename with a unique tmp name, so the
# heartbeat loop and the main flow can both write status.json without ever
# leaving torn JSON for the viewer to read.
# ---------------------------------------------------------------------------
json_field() { # json_field <file> <dotted.key>  (empty on any failure)
  python3 -c "
import json, sys
try:
    v = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)
for k in sys.argv[2].split('.'):
    if not isinstance(v, dict):
        sys.exit(0)
    v = v.get(k)
    if v is None:
        sys.exit(0)
if isinstance(v, (dict, list)):
    sys.stdout.write(json.dumps(v))
else:
    sys.stdout.write(str(v))
" "$1" "$2" 2>/dev/null || true
}

# write_status <state> <stage> [<error>] — keeps started_ts/phase from the
# existing record; refreshes updated_ts. This one writer is also the
# heartbeat (same state, fresh timestamp).
write_status() {
  STATE="$1" STAGE="$2" ERROR="${3:-}" SLUG="$slug" PHASE="${STATUS_PHASE:-}" \
    python3 - "$STATUS_JSON" <<'PY'
import json, os, sys, time
from pathlib import Path
target = Path(sys.argv[1])
try:
    prev = json.loads(target.read_text(encoding="utf-8"))
    if not isinstance(prev, dict):
        prev = {}
except Exception:
    prev = {}
now = int(time.time())
record = {
    "slug": os.environ["SLUG"],
    "state": os.environ["STATE"],
    "stage": os.environ["STAGE"],
    "phase": os.environ.get("PHASE") or prev.get("phase"),
    "started_ts": prev.get("started_ts") or now,
    "updated_ts": now,
    "pid": os.getppid(),
}
err = os.environ.get("ERROR", "")
if err:
    record["error"] = err
tmp = target.with_name(f"{target.name}.tmp.{os.getpid()}")
tmp.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
tmp.replace(target)
PY
}

session_alive() { # exact-name match: has-session does prefix matching
  tmux_cmd list-sessions -F '#S' 2>/dev/null | grep -Fxq "$1"
}

RUNNING_STATES="intake resolving building initing launching verifying"
FAILED_STATES="resolve_failed resolve_stale build_failed init_failed launch_failed"

state_now() { json_field "$STATUS_JSON" state; }

state_is_running() {
  local s
  for s in $RUNNING_STATES; do [[ "$1" == "$s" ]] && return 0; done
  return 1
}

state_is_failed() {
  local s
  for s in $FAILED_STATES; do [[ "$1" == "$s" ]] && return 0; done
  return 1
}

refuse_if_phase_running() {
  if session_alive "$SESSION"; then
    die "a phase is already running for '$slug' (tmux session $SESSION). Wait, or kill the session first."
  fi
}

# ---------------------------------------------------------------------------
# Phase spawning. The caller's relevant environment is snapshotted into
# phase_env.sh (tmux panes inherit the SERVER's env, not ours), and the pane
# runs run_phase.sh, which sources the snapshot and appends everything to
# create.log. A fresh snapshot is taken on every spawn, so a retry issued by
# the viewer runs under the viewer's current environment.
# ---------------------------------------------------------------------------
# Validate the template overrides at CLAIM time, not at build time. A bad
# model name or a malformed remote URL should be a 4xx the operator sees
# while the form is still in front of them — not a phase-B failure twenty
# minutes into a mathlib fetch.
validate_overrides() {
  local spec role value
  if [[ -n "$MODEL_ARG" ]]; then
    [[ "$MODEL_ARG" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] \
      || die "--model must match [A-Za-z0-9][A-Za-z0-9._-]* (got: $MODEL_ARG)"
  fi
  if [[ -n "$EFFORT_ARG" ]]; then
    # Deliberately a charset check, not an allowlist: efforts are
    # provider-specific (codex xhigh, claude max, ...) and a hardcoded list
    # here would rot the first time a provider adds a tier.
    [[ "$EFFORT_ARG" =~ ^[a-z][a-z0-9_-]*$ ]] \
      || die "--effort must match [a-z][a-z0-9_-]* (got: $EFFORT_ARG)"
  fi
  for spec in ${ROLE_MODEL_SPECS[@]+"${ROLE_MODEL_SPECS[@]}"}; do
    [[ "$spec" == *=* ]] || die "--role-model expects <config-role>=<model> (got: $spec)"
    role="${spec%%=*}"
    value="${spec#*=}"
    [[ "$role" =~ ^[A-Za-z][A-Za-z0-9_.]*$ ]] \
      || die "--role-model role must match [A-Za-z][A-Za-z0-9_.]* (got: $role)"
    [[ "$value" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] \
      || die "--role-model model must match [A-Za-z0-9][A-Za-z0-9._-]* (got: $value)"
  done
  for spec in ${ROLE_EFFORT_SPECS[@]+"${ROLE_EFFORT_SPECS[@]}"}; do
    [[ "$spec" == *=* ]] || die "--role-effort expects <config-role>=<effort> (got: $spec)"
    role="${spec%%=*}"
    value="${spec#*=}"
    [[ "$role" =~ ^[A-Za-z][A-Za-z0-9_.]*$ ]] \
      || die "--role-effort role must match [A-Za-z][A-Za-z0-9_.]* (got: $role)"
    [[ "$value" =~ ^[a-z][a-z0-9_-]*$ ]] \
      || die "--role-effort effort must match [a-z][a-z0-9_-]* (got: $value)"
  done
  if [[ -n "$GRUNT_WALL_ARG" ]]; then
    # 300s floor: below the observed cost of a single compile round-trip a
    # grunt cannot finish anything, so a smaller wall only burns slots.
    [[ "$GRUNT_WALL_ARG" =~ ^[0-9]+$ ]] && [ "$GRUNT_WALL_ARG" -ge 300 ] && [ "$GRUNT_WALL_ARG" -le 21600 ] \
      || die "--grunt-wall must be an integer 300-21600 seconds (got: $GRUNT_WALL_ARG)"
  fi
  if [[ -n "$GRUNTS_ARG" && "$GRUNTS_ARG" != "off" ]]; then
    [[ "$GRUNTS_ARG" =~ ^[1-9][0-9]?$ ]] \
      || die "--grunts must be 'off' or 1-99 (got: $GRUNTS_ARG)"
  fi
  if [[ -n "$REMOTE_URL_ARG" ]]; then
    # Accept the two shapes setup writes into config.git.remote_url; reject
    # anything carrying shell or newline payload, since this string is
    # written into a config the supervisor later hands to git.
    [[ "$REMOTE_URL_ARG" =~ ^(git@[A-Za-z0-9._-]+:[A-Za-z0-9._/-]+(\.git)?|https://[A-Za-z0-9._-]+/[A-Za-z0-9._/-]+(\.git)?)$ ]] \
      || die "--remote-url must be git@host:owner/repo(.git) or https://host/owner/repo(.git) (got: $REMOTE_URL_ARG)"
  fi
}

# Write a job-local template carrying the operator's overrides, and echo its
# path. Mirrors the historical launcher's scrub step: the chosen template is data,
# the overrides are the operator's, and setup only ever sees the merged
# result via CONFIG_TEMPLATE. Re-derived on every phase-B spawn so a retry
# after an edited job.json picks the new values up.
derive_job_template() {
  local base="$1" out="$JOB_DIR/config-template.json"
  J_T_MODEL="$(json_field "$JOB_JSON" model)" \
  J_T_EFFORT="$(json_field "$JOB_JSON" effort)" \
  J_T_ROLE_OVERRIDES="$(json_field "$JOB_JSON" role_overrides)" \
  J_T_GRUNTS="$(json_field "$JOB_JSON" grunts)" \
  J_T_GRUNT_WALL="$(json_field "$JOB_JSON" grunt_wall)" \
  J_T_REMOTE="$(json_field "$JOB_JSON" remote_url)" \
  python3 - "$base" "$out" <<'PY'
import json, os, sys
from pathlib import Path

data = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))

model = os.environ.get("J_T_MODEL", "").strip()
effort = os.environ.get("J_T_EFFORT", "").strip()
try:
    role_overrides = json.loads(os.environ.get("J_T_ROLE_OVERRIDES", "") or "{}")
except json.JSONDecodeError:
    raise SystemExit("job role_overrides is not valid JSON")
if not isinstance(role_overrides, dict):
    raise SystemExit("job role_overrides must be an object")
grunts = os.environ.get("J_T_GRUNTS", "").strip()
wall = os.environ.get("J_T_GRUNT_WALL", "").strip()
remote = os.environ.get("J_T_REMOTE", "").strip()

# Lanes: override only what was asked for, so a template that deliberately
# differentiates a lane keeps its shape unless the operator overrides it.
# Every agent the run dispatches, not a hand-listed few. The top-level
# lanes are worker/easy_worker/hard_worker/reviewer, but `verification`
# holds per-lane agent POOLS (correspondence_agents, soundness_agents,
# substantiveness_agents) — and those bursts log as role "reviewer", so a
# stale model in a pool is invisible in a role tally. Listing lanes by hand
# missed them entirely: a run asked for gpt-5.6-sol dispatched 107 of 141
# reviewer-role bursts on gpt-5.5 (58 corr, 28 sound, 21 substantiveness).
#
# Walk the config and rewrite every `model`/`effort` an agent spec carries,
# so a block added later is covered without editing this list again.
#
# The kernel ALSO honours root-level `blockered_worker`,
# `easy_close_worker`, `stuck_math_audit`, `worker_rules[].binding`, and
# `workflow.phase_overrides.*`. They are deliberately absent from this
# list: no SHIPPED TEMPLATE carries them. Hand-edited run configs do --
# live runs have pinned `stuck_math_audit`, and others have set
# `blockered_worker`, `easy_close_worker` and `worker_rules` -- so absence
# here reflects the templates, not the
# absence of demand. `test_config_template_model_coverage.py` fails the
# moment a shipped template starts carrying one, which is the signal to
# extend this tuple rather than a reason to pre-emptively widen it.
AGENT_ROOTS = ("worker", "easy_worker", "hard_worker", "reviewer", "stuck_math_audit", "verification")
# NOTE: the shipped templates now set `stuck_math_audit` explicitly, so a
# per-role `--role-model reviewer=X` no longer cascades to the audit lane
# the way it did when the key was absent and the kernel fell back to
# reviewer. Global --model/--effort still reach it, via this tuple. The
# browser wizard always submits every role, so it shows the audit lane
# explicitly; only the CLI per-role path is affected.


def _apply_agent_overrides(obj):
    if isinstance(obj, dict):
        # An agent spec is anything naming a model; `provider` distinguishes
        # it from unrelated dicts that happen to have a "model" key.
        if "model" in obj and "provider" in obj:
            if model:
                obj["model"] = model
            if effort and "effort" in obj:
                obj["effort"] = effort
        for value in obj.values():
            _apply_agent_overrides(value)
    elif isinstance(obj, list):
        for value in obj:
            _apply_agent_overrides(value)

if model or effort:
    for root in AGENT_ROOTS:
        if root in data:
            _apply_agent_overrides(data[root])

# Per-role overrides use the config path emitted by the viewer's generic
# schema walk. A list index is intentionally omitted, so one verifier-pool
# control updates every configured agent in that role. Walk the whole config:
# a future model-bearing role in a shipped template works without adding its
# name here or to another AGENT_ROOTS-style list.
def _apply_role_overrides(obj, path=""):
    if isinstance(obj, dict):
        selected = role_overrides.get(path)
        if "model" in obj and "provider" in obj and isinstance(selected, dict):
            selected_model = str(selected.get("model") or "").strip()
            selected_effort = str(selected.get("effort") or "").strip()
            if selected_model:
                obj["model"] = selected_model
            if selected_effort:
                obj["effort"] = selected_effort
        for key, value in obj.items():
            child = f"{path}.{key}" if path else key
            _apply_role_overrides(value, child)
    elif isinstance(obj, list):
        for value in obj:
            _apply_role_overrides(value, path)

if role_overrides:
    _apply_role_overrides(data)

# The daemon reads these from SUB-BLOCKS, not the top level:
# `sidecar.daemon.grunts` and `sidecar.budgets.attempt_wall_seconds`
# (trellis/sidecar/config.py `_sub("daemon")` / `_sub("budgets")`). Only
# `enabled` is top-level. Written flat they parse as unknown keys and are
# silently ignored, so the run quietly takes the loader defaults — which is
# exactly what happened: a config saying grunts=2/wall=5400 ran a pool of 2
# on a 2700s wall purely because those were the defaults of the day.
if grunts:
    sidecar = data.setdefault("sidecar", {})
    if grunts == "off":
        sidecar["enabled"] = False
    else:
        sidecar["enabled"] = True
        sidecar.setdefault("daemon", {})["grunts"] = int(grunts)
        # A pool with no wall budget is not a working pool; supply the
        # template's value or the known-good default rather than leaving
        # the sidecar to guess.
        sidecar.setdefault("budgets", {}).setdefault("attempt_wall_seconds", 3600)

if wall:
    # Applies whether or not --grunts was given: a template that enables the
    # sidecar should still honour an explicit wall.
    data.setdefault("sidecar", {}).setdefault("budgets", {})[
        "attempt_wall_seconds"
    ] = int(wall)

if remote:
    git_cfg = data.setdefault("git", {})
    git_cfg["remote_url"] = remote

Path(sys.argv[2]).write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
PY
  printf '%s\n' "$out"
}

write_phase_env() {
  python3 - "$PHASE_ENV" <<'PY'
import os, re, shlex, sys
from pathlib import Path
keep = re.compile(
    r"^(PATH|HOME|TRELLIS_PROJECTS_ROOT|TRELLIS_TMUX_SOCKET|"
    r"TRELLIS_TRELLIS_KERNEL_CMD|TRELLIS_CREATE_[A-Z_]+|"
    r"TRELLIS_SOUNDNESS_FINGERPRINT_MODE|TRELLIS_CSC_[A-Z_]+|"
    r"BURST_[A-Z_]+|MATHLIB_[A-Z_]+|ELAN_HOME|STATIC_OUT|"
    r"SETUP_SCRATCH_ROOT|PYTHONPATH)$"
)
# Never forward a checker socket: phase B starts its own checker, and a stale
# TRELLIS_CHECKER_SOCKET from the caller would point setup's optional inline
# check at another run's server.
lines = ["# Generated by trellis_create_run.sh; sourced by run_phase.sh.", ""]
for name in sorted(os.environ):
    if keep.match(name) and name != "TRELLIS_CHECKER_SOCKET":
        lines.append(f"export {name}={shlex.quote(os.environ[name])}")
Path(sys.argv[1]).write_text("\n".join(lines) + "\n", encoding="utf-8")
PY
  cat > "$PHASE_LAUNCHER" <<LAUNCHER
#!/usr/bin/env bash
# Generated by trellis_create_run.sh. Runs one phase with the recorded
# environment, appending to create.log.
set -uo pipefail
source '$PHASE_ENV'
exec bash '$SCRIPT_PATH' "\$1" '$slug' >> '$CREATE_LOG' 2>&1
LAUNCHER
  chmod +x "$PHASE_LAUNCHER"
}

# Row 28: create.log grows without bound across many retries of a long build.
# Rotation happens only HERE — between phases, never mid-append: a running
# phase holds an open O_APPEND fd on the log, and renaming underneath it would
# silently split its output. Keep-last policy: the previous log survives at
# create.log.1, and the fresh log opens with a notice naming it.
rotate_create_log() {
  local size
  size="$(stat -c %s "$CREATE_LOG" 2>/dev/null || echo 0)"
  if [[ "$size" -gt "$LOG_ROTATE_BYTES" ]]; then
    mv "$CREATE_LOG" "$CREATE_LOG.1"
    printf '[create.log rotated: earlier output (%s bytes) kept at create.log.1]\n' \
      "$size" > "$CREATE_LOG"
  fi
}

spawn_phase() { # spawn_phase <phase-a|phase-b>
  rotate_create_log
  write_phase_env
  tmux_cmd new-session -d -s "$SESSION" "bash '$PHASE_LAUNCHER' $1"
}

# Heartbeat: refresh updated_ts while a phase runs. Started at phase entry,
# killed by the phase's exit trap. Reads the CURRENT state back off disk so a
# late beat can never resurrect a state the main flow has already left.
HEARTBEAT_PID=""
start_heartbeat() {
  (
    while :; do
      sleep "$HEARTBEAT_SECS"
      current_state="$(state_now)"
      current_stage="$(json_field "$STATUS_JSON" stage)"
      [[ -n "$current_state" ]] || continue
      write_status "$current_state" "$current_stage" "$(json_field "$STATUS_JSON" error)"
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

# fail_phase <state> <stage> <message> — record the failure and exit the
# phase. The trap in each phase routes unexpected errors here too.
fail_phase() {
  stop_heartbeat
  write_status "$1" "$2" "$3"
  echo "trellis-create: $3" >&2
  exit 1
}

# Copy a paper into the job dir as UTF-8. Non-UTF-8 input gets the
# add_reference_paper.sh transcode treatment (windows-1252, then latin-1) —
# closing the R7 asymmetry for the primary paper (design §4, row 3). Prints
# the source encoding ("utf-8" when no transcode happened).
ingest_paper() { # ingest_paper <src> <dst>
  python3 - "$1" "$2" <<'PY'
import sys
from pathlib import Path
data = Path(sys.argv[1]).read_bytes()
for encoding in ("utf-8", "cp1252", "latin-1"):
    try:
        text = data.decode(encoding)
    except UnicodeDecodeError:
        continue
    Path(sys.argv[2]).write_text(text, encoding="utf-8")
    print(encoding)
    sys.exit(0)
raise SystemExit("paper cannot be decoded as UTF-8, cp1252 or latin-1")
PY
}

# Ingest every --reference spec into the job dir (so phase B never depends on
# the caller's paths surviving the wait at awaiting_targets), validating ids
# (row 25: bad id / duplicate id / bad encoding all fail HERE, before any
# state is recorded). Consumes REFERENCE_SPECS; fills REF_SPECS_REWRITTEN with
# `<id>=$JOB_DIR/refs/<id>.tex:<source_id>` specs and prunes refs/ files that
# are no longer named (a replaced set must not leave stale uploads behind).
# Prints its own error and returns 1 — the caller decides whether a failure
# also tears down the job dir (start does; resolve must not).
REF_SPECS_REWRITTEN=()
ingest_reference_specs() {
  REF_SPECS_REWRITTEN=()
  local spec ref_id rest ref_file ref_source seen_ids=" "
  for spec in ${REFERENCE_SPECS[@]+"${REFERENCE_SPECS[@]}"}; do
    ref_id="${spec%%=*}"
    rest="${spec#*=}"
    if [ -f "$rest" ]; then ref_file="$rest"; ref_source="$ref_id"
    elif [[ "$rest" == *:* ]]; then ref_file="${rest%:*}"; ref_source="${rest##*:}"
    else ref_file="$rest"; ref_source="$ref_id"; fi
    if [[ ! "$ref_id" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
      echo "trellis-create: --reference id must match [A-Za-z0-9][A-Za-z0-9._-]* (got: $ref_id)" >&2
      return 1
    fi
    if [[ "$seen_ids" == *" $ref_id "* ]]; then
      echo "trellis-create: duplicate --reference id '$ref_id' — reference ids are unique per run" >&2
      return 1
    fi
    seen_ids="$seen_ids$ref_id "
    if [[ ! -f "$ref_file" ]]; then
      echo "trellis-create: --reference file not found: $ref_file" >&2
      return 1
    fi
    mkdir -p "$JOB_DIR/refs"
    if ! ingest_paper "$ref_file" "$JOB_DIR/refs/$ref_id.tex" >/dev/null; then
      echo "trellis-create: reference '$ref_id' is not decodable text: $ref_file" >&2
      return 1
    fi
    REF_SPECS_REWRITTEN+=("$ref_id=$JOB_DIR/refs/$ref_id.tex:$ref_source")
  done
  local existing base
  for existing in "$JOB_DIR"/refs/*.tex; do
    [[ -e "$existing" ]] || continue
    base="$(basename "$existing" .tex)"
    [[ "$seen_ids" == *" $base "* ]] || rm -f "$existing"
  done
  return 0
}

# The kernel CLI for the resolver's mirror-drift guard: the env override, else
# a prebuilt binary. Never `cargo run` from here — a create job must not spend
# minutes compiling a kernel that setup will need anyway; without a binary the
# resolver runs mirror-only and phase B's setup fails loudly instead.
resolve_kernel_cmd() {
  if [[ -n "${TRELLIS_TRELLIS_KERNEL_CMD:-}" ]]; then
    printf '%s' "$TRELLIS_TRELLIS_KERNEL_CMD"
    return 0
  fi
  local candidate
  for candidate in "$ROOT_DIR/kernel/target/release/trellis_runtime_cli" \
                   "$ROOT_DIR/kernel/target/debug/trellis_runtime_cli"; do
    if [[ -x "$candidate" ]]; then
      printf '%s' "$candidate"
      return 0
    fi
  done
  return 1
}

# ---------------------------------------------------------------------------
# job.json read/update helpers (python owns the JSON surgery; bash carries
# opaque values only).
# ---------------------------------------------------------------------------
job_update() { # job_update  — reads env: J_* variables set by callers
  python3 - "$JOB_JSON" <<'PY'
import json, os, sys, time
from pathlib import Path
target = Path(sys.argv[1])
try:
    job = json.loads(target.read_text(encoding="utf-8"))
except Exception:
    job = {}

def env(name):
    return os.environ.get(name)

if env("J_INIT") == "1":
    job = {
        "slug": env("J_SLUG"),
        "created_ts": int(time.time()),
        "paper": env("J_PAPER"),
        "paper_name": env("J_PAPER_NAME"),
        "paper_transcoded_from": env("J_TRANSCODED") or None,
        "loogle": env("J_LOOGLE"),
        "template": env("J_TEMPLATE") or None,
        # Template overrides. None means "inherit the template" — an absent
        # override must never be confused with an explicit one, which is why
        # these are stored as null rather than "".
        "remote_url": env("J_REMOTE_URL") or None,
        "model": env("J_MODEL") or None,
        "effort": env("J_EFFORT") or None,
        "role_overrides": json.loads(env("J_ROLE_OVERRIDES") or "{}"),
        "grunts": env("J_GRUNTS") or None,
        "grunt_wall": env("J_GRUNT_WALL") or None,
        "env_map": json.loads(env("J_ENV_MAP") or "[]"),
        "main_result_envs": env("J_MAIN_RESULT_ENVS") or None,
        "references": json.loads(env("J_REFERENCES") or "[]"),
        "repo": env("J_REPO"),
        "runtime_root": env("J_RUNTIME_ROOT"),
        "selected": None,
    }
else:
    if env("J_ENV_MAP") is not None:
        job["env_map"] = json.loads(env("J_ENV_MAP"))
    if env("J_MAIN_RESULT_ENVS") is not None:
        job["main_result_envs"] = env("J_MAIN_RESULT_ENVS") or None
    if env("J_TRANSCODED") is not None:
        job["paper_transcoded_from"] = env("J_TRANSCODED") or None
    if env("J_REFERENCES") is not None:
        job["references"] = json.loads(env("J_REFERENCES"))
    if env("J_SELECTED") is not None:
        job["selected"] = json.loads(env("J_SELECTED"))
    if env("J_CLEAR_SELECTION") == "1":
        job["selected"] = None

tmp = target.with_name(f"{target.name}.tmp.{os.getpid()}")
tmp.write_text(json.dumps(job, indent=2, sort_keys=True) + "\n", encoding="utf-8")
tmp.replace(target)
PY
}

# Parse repeated --env-map/--main-result-envs/--paper flags shared by
# start/resolve.
ENV_MAP_SPECS=()
MAIN_RESULT_ENVS_ARG=""
MAIN_RESULT_ENVS_SET=0
PAPER_ARG=""
LOOGLE_ARG=""
TEMPLATE_ARG=""
REMOTE_URL_ARG=""
MODEL_ARG=""
EFFORT_ARG=""
ROLE_MODEL_SPECS=()
ROLE_EFFORT_SPECS=()
GRUNTS_ARG=""
GRUNT_WALL_ARG=""
REFERENCE_SPECS=()
CLEAR_REFERENCES=0
SELECT_KEYS=()
CONFIRM_PATHS=()
parse_flags() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --paper)             PAPER_ARG="${2:?--paper requires a path}"; shift 2 ;;
      --loogle)            LOOGLE_ARG="${2:?--loogle requires on|off}"; shift 2 ;;
      --template)          TEMPLATE_ARG="${2:?--template requires a path}"; shift 2 ;;
      # Template overrides. Each is applied to a JOB-LOCAL copy of the
      # template (see `derive_job_template`), never to the shared file in
      # examples/ — two concurrent creations must not edit each other's
      # config, and the template on disk stays the documented default.
      --remote-url)        REMOTE_URL_ARG="${2?--remote-url requires a URL}"; shift 2 ;;
      --model)             MODEL_ARG="${2:?--model requires a model name}"; shift 2 ;;
      --effort)            EFFORT_ARG="${2:?--effort requires an effort}"; shift 2 ;;
      --role-model)        ROLE_MODEL_SPECS+=("${2:?--role-model requires role=model}"); shift 2 ;;
      --role-effort)       ROLE_EFFORT_SPECS+=("${2:?--role-effort requires role=effort}"); shift 2 ;;
      # `off` disables the closure sidecar; a positive integer enables it
      # with that many grunts.
      --grunts)            GRUNTS_ARG="${2:?--grunts requires off or a positive integer}"; shift 2 ;;
      # Per-attempt wall-clock cap for a grunt, in seconds. The wall is a
      # hard kill, so this is the single knob that decides how long a dead
      # end may hold a pool slot.
      --grunt-wall)        GRUNT_WALL_ARG="${2:?--grunt-wall requires seconds}"; shift 2 ;;
      --env-map)           ENV_MAP_SPECS+=("${2:?--env-map requires ALIAS=CANONICAL}"); shift 2 ;;
      # `?` not `:?`: an EMPTY list is legal and means "back to the kernel
      # default set" (the absent-config-key path) — only a missing argument
      # is an error.
      --main-result-envs)  MAIN_RESULT_ENVS_ARG="${2?--main-result-envs requires an argument}"; MAIN_RESULT_ENVS_SET=1; shift 2 ;;
      --reference)         REFERENCE_SPECS+=("${2:?--reference requires <id>=<file>[:<source_id>]}"); shift 2 ;;
      --clear-references)  CLEAR_REFERENCES=1; shift ;;
      --select)            SELECT_KEYS+=("${2:?--select requires a candidate key}"); shift 2 ;;
      --confirm)           CONFIRM_PATHS+=("${2:?--confirm requires a path}"); shift 2 ;;
      *) die "unknown option: $1" ;;
    esac
  done
}

env_map_json() {
  python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' \
    ${ENV_MAP_SPECS[@]+"${ENV_MAP_SPECS[@]}"}
}

role_overrides_json() {
  python3 - ${ROLE_MODEL_SPECS[@]+"${ROLE_MODEL_SPECS[@]}"} -- \
    ${ROLE_EFFORT_SPECS[@]+"${ROLE_EFFORT_SPECS[@]}"} <<'PY'
import json, sys

split = sys.argv.index("--")
models = sys.argv[1:split]
efforts = sys.argv[split + 1:]
roles = {}
for field, specs in (("model", models), ("effort", efforts)):
    for spec in specs:
        role, value = spec.split("=", 1)
        roles.setdefault(role, {})[field] = value
print(json.dumps(roles, separators=(",", ":")))
PY
}

require_job() {
  [[ -d "$JOB_DIR" && -f "$JOB_JSON" ]] \
    || die "no create job for '$slug' (expected $JOB_JSON)"
}

# ===========================================================================
# PHASE A — intake + resolve, then park with no process (design §4).
# ===========================================================================
run_phase_a() {
  STATUS_PHASE=a
  trap 'stop_heartbeat' EXIT
  write_status intake intake
  start_heartbeat

  local paper="$JOB_DIR/paper.tex"
  [[ -f "$paper" ]] || fail_phase resolve_failed intake "job has no paper at $paper"

  write_status resolving resolve

  local resolve_cmd="${TRELLIS_CREATE_RESOLVE_CMD:-python3 $ROOT_DIR/scripts/resolve_paper_targets.py}"
  local args=("$paper" --out "$RESOLUTION_JSON.tmp" --paper-name "$(json_field "$JOB_JSON" paper_name)")
  local envs spec
  envs="$(json_field "$JOB_JSON" main_result_envs)"
  [[ -n "$envs" ]] && args+=(--main-result-envs "$envs")
  while IFS= read -r spec; do
    [[ -n "$spec" ]] && args+=(--env-map "$spec")
  done < <(python3 -c 'import json,sys
try:
    job = json.load(open(sys.argv[1]))
except Exception:
    job = {}
for entry in job.get("env_map") or []:
    print(entry)' "$JOB_JSON")
  local kernel_cmd
  if kernel_cmd="$(resolve_kernel_cmd)"; then
    args+=(--kernel-cmd "$kernel_cmd")
  else
    echo "trellis-create: no prebuilt kernel CLI found; resolving mirror-only (the kernel-agreement guard is skipped)"
  fi

  if ! $resolve_cmd "${args[@]}"; then
    fail_phase resolve_failed resolve "target resolution failed — see create.log"
  fi
  # Keep what the user last saw: the viewer diffs prev-vs-current so a
  # re-upload/re-resolve can say "+2 candidates, thm:aux disappeared" instead
  # of silently re-rendering (design §5.5 item 3, row 29's swallow warning).
  if [[ -f "$RESOLUTION_JSON" ]]; then
    cp "$RESOLUTION_JSON" "$PREV_RESOLUTION_JSON"
  fi
  mv "$RESOLUTION_JSON.tmp" "$RESOLUTION_JSON"

  stop_heartbeat
  write_status awaiting_targets awaiting_targets
  echo "trellis-create: awaiting_targets — $RESOLUTION_JSON written; confirm a selection to build."
}

# ===========================================================================
# PHASE B — build, init, launch, verify (design §4, §5.6).
# ===========================================================================

# Selected candidate keys -> the kernel's raw_targets wire shape, BY LOOKUP
# into targets_resolution.json rather than by parsing the key: a tex label is
# any string (R4), so a label that happens to look like `lines:5-7` must stay
# a label. A labeled candidate becomes its label string; an unlabeled one
# becomes {start_line, end_line}. A selected key with no candidate is the
# resolve_stale case (row 10): exit 4, distinct from any other failure, so
# phase B can record the dedicated `resolve_stale` state rather than a
# generic build_failed.
translate_selection() {
  python3 - "$JOB_JSON" "$RESOLUTION_JSON" "$SELECTION_JSON" <<'PY'
import json, os, sys
from pathlib import Path
job = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
resolution = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8"))
selected = job.get("selected") or []
if not selected:
    raise SystemExit("no selection recorded in job.json — confirm targets first")
by_key = {c.get("key"): c for c in resolution.get("candidates") or []}
targets = []
missing = []
for key in selected:
    candidate = by_key.get(key)
    if candidate is None:
        missing.append(key)
        continue
    if candidate.get("tex_label"):
        targets.append(candidate["tex_label"])
    else:
        targets.append(
            {"start_line": candidate["start_line"], "end_line": candidate["end_line"]}
        )
if missing:
    sys.stderr.write(
        "resolve_stale: selected keys not in the current resolution: "
        + ", ".join(missing)
        + " — re-resolve and re-confirm.\n"
    )
    raise SystemExit(4)
out = Path(sys.argv[3])
tmp = out.with_name(out.name + f".tmp.{os.getpid()}")
tmp.write_text(json.dumps(targets, indent=2) + "\n", encoding="utf-8")
tmp.replace(out)
PY
}

capture_pane_tail() { # capture_pane_tail <session> <label>
  if session_alive "$1"; then
    echo "----- last output of tmux session $1 ($2) -----"
    tmux_cmd capture-pane -p -t "$1" 2>/dev/null | tail -30 || true
    echo "-----------------------------------------------"
  fi
}

run_phase_b() {
  STATUS_PHASE=b
  trap 'stop_heartbeat' EXIT
  write_status building setup
  start_heartbeat

  # First repo write: the marker. From here until launch verifies, the viewer
  # classifies this repo as `creating`, never as a run.
  mkdir -p "$REPO"
  touch "$MARKER"

  local translate_rc=0
  translate_selection || translate_rc=$?
  if [[ "$translate_rc" -eq 4 ]]; then
    # Row 10: the selection names candidates the current resolution no longer
    # holds. Its own state, its own recovery — re-resolve, then re-confirm
    # (the mathlib prewarm survives via the stage ledger's inputs_sha).
    fail_phase resolve_stale setup "selection is stale against the current resolution — re-resolve and re-confirm (see create.log)"
  elif [[ "$translate_rc" -ne 0 ]]; then
    fail_phase build_failed setup "selection could not be translated — see create.log"
  fi

  # Row 14 preflight: the S9 prewarm writes GBs; say so BEFORE it starts
  # rather than letting ENOSPC name the problem an hour in. Warn, never block
  # — the operator may know better (a tar seed, a nearly-warm .lake).
  local free_kb warn_kb
  free_kb="$(df -Pk "$PROJECTS_ROOT" 2>/dev/null | awk 'NR==2 {print $4}')"
  warn_kb=$((DISK_WARN_GB * 1024 * 1024))
  if [[ -n "$free_kb" && "$free_kb" -lt "$warn_kb" ]]; then
    echo "trellis-create: WARNING — only $((free_kb / 1024 / 1024)) GB free on $PROJECTS_ROOT (below ${DISK_WARN_GB} GB); the mathlib prewarm alone can need more. If the build fails with ENOSPC, free space and retry — every stage is idempotent under re-run."
  fi

  local setup_cmd="${TRELLIS_CREATE_SETUP_CMD:-bash $ROOT_DIR/scripts/setup_repo.sh}"
  local setup_args=(--yes --resume)
  # A second attempt after a re-resolve legitimately changes the pinned
  # inputs (a new selection sha, a different env map) — that change came
  # through the confirm gate, which IS the deliberate adoption setup's
  # --reconfigure asks for. First attempts have no ledger and take the plain
  # resume path.
  if [[ -f "$REPO/.trellis/setup_stages.json" ]]; then
    setup_args+=(--reconfigure)
  fi
  local loogle envs spec
  loogle="$(json_field "$JOB_JSON" loogle)"
  [[ -n "$loogle" ]] || fail_phase build_failed setup "job.json has no loogle setting"
  setup_args+=(--loogle "$loogle" --targets-json "$SELECTION_JSON")
  envs="$(json_field "$JOB_JSON" main_result_envs)"
  [[ -n "$envs" ]] && setup_args+=(--main-result-envs "$envs")
  while IFS= read -r spec; do
    [[ -n "$spec" ]] && setup_args+=(--env-map "$spec")
  done < <(python3 -c 'import json,sys
try:
    job = json.load(open(sys.argv[1]))
except Exception:
    job = {}
for entry in job.get("env_map") or []:
    print(entry)' "$JOB_JSON")
  while IFS= read -r spec; do
    [[ -n "$spec" ]] && setup_args+=(--reference "$spec")
  done < <(python3 -c 'import json,sys
try:
    job = json.load(open(sys.argv[1]))
except Exception:
    job = {}
for entry in job.get("references") or []:
    print(entry)' "$JOB_JSON")
  setup_args+=("$REPO" "$JOB_DIR/paper.tex" "$slug")

  # Setup's output streams to create.log as before (the wizard's tail follows
  # a long S9 live) AND lands in a scratch copy, because §5.6's other
  # resolve_stale route lives inside setup: a paper changed between confirm
  # and build fails the kernel's match_block re-resolve with a signature we
  # must route to `resolve_stale`, not a generic build_failed. tee + PIPESTATUS
  # under `set +e` — with pipefail on, a raw failing pipeline would exit the
  # phase through the trap without ever recording the failure.
  local template setup_out setup_rc
  template="$(json_field "$JOB_JSON" template)"
  # Overrides are applied to a job-local copy; when any is present we hand
  # setup that copy instead. With no template chosen we still need a base to
  # merge onto, so fall back to the repo's default example — the same file
  # the endpoint marks as default.
  local role_overrides_json_value
  role_overrides_json_value="$(json_field "$JOB_JSON" role_overrides)"
  if [[ -n "$(json_field "$JOB_JSON" model)$(json_field "$JOB_JSON" effort)$(json_field "$JOB_JSON" grunts)$(json_field "$JOB_JSON" grunt_wall)$(json_field "$JOB_JSON" remote_url)" \
        || ( -n "$role_overrides_json_value" && "$role_overrides_json_value" != "{}" ) ]]; then
    local base="${template:-$ROOT_DIR/examples/trellis.config.json}"
    if [[ -f "$base" ]]; then
      template="$(derive_job_template "$base")"
      echo "trellis-create: applied config overrides to a job-local template: $template"
    else
      echo "trellis-create: WARNING — no base template at $base; config overrides were NOT applied"
    fi
  fi
  setup_out="$JOB_DIR/.last_setup_output"
  set +e
  if [[ -n "$template" ]]; then
    CONFIG_TEMPLATE="$template" $setup_cmd "${setup_args[@]}" 2>&1 | tee "$setup_out"
  else
    $setup_cmd "${setup_args[@]}" 2>&1 | tee "$setup_out"
  fi
  setup_rc=${PIPESTATUS[0]}
  set -e
  if [[ "$setup_rc" -ne 0 ]]; then
    if grep -q "Could not locate paper text" "$setup_out"; then
      fail_phase resolve_stale setup "the paper changed between confirm and build (the kernel could not locate the selected text) — re-resolve and re-confirm"
    fi
    fail_phase build_failed setup "setup_repo.sh failed — see create.log"
  fi
  rm -f "$setup_out"

  # --- init: wipe-redo (design §3.2). The runtime root holds nothing
  # irreplaceable BEFORE first launch, and that precondition is checked, not
  # assumed (mirrors the historical launcher): anything beyond protocol_state.json +
  # runtime_metadata.json means a run has been here, and the wipe refuses.
  write_status initing init
  if [[ -e "$RUNTIME_ROOT" ]]; then
    local unexpected
    unexpected="$(find "$RUNTIME_ROOT" -mindepth 1 -maxdepth 1 \
      ! -name protocol_state.json ! -name runtime_metadata.json \
      -printf '%f\n' 2>/dev/null | sort | head -5 | tr '\n' ' ')"
    if [[ -n "$unexpected" ]]; then
      fail_phase init_failed init "runtime root $RUNTIME_ROOT is not fresh (holds: $unexpected) — a launched run's state is never wiped from here"
    fi
    rm -rf "$RUNTIME_ROOT"
  fi
  local init_cmd="${TRELLIS_CREATE_INIT_CMD:-bash $ROOT_DIR/scripts/trellis.sh init}"
  if ! $init_cmd "$REPO/trellis.config.json" "$RUNTIME_ROOT"; then
    fail_phase init_failed init "trellis.sh init failed — see create.log"
  fi

  # --- launch: checker session -> socket wait -> run session. The same
  # sequence restart_configured_run.sh performs, WITHOUT its wipe path —
  # that script is never a viewer code path (design §10 item 6).
  write_status launching launch
  tmux_cmd kill-session -t "$RUN_SESSION" 2>/dev/null || true
  tmux_cmd kill-session -t "$CHECKER_SESSION" 2>/dev/null || true
  # A retry relaunches the run, so a sidecar from the previous attempt must
  # not survive pointing at it. `drain` would be gentler, but there is no
  # in-flight work worth preserving on a path that is restarting the run.
  tmux_cmd kill-session -t "$SIDECAR_SESSION" 2>/dev/null || true
  rm -f "$CHECKER_SOCKET"
  local checker_cmd="${TRELLIS_CREATE_CHECKER_CMD:-}"
  if [[ -n "$checker_cmd" ]]; then
    tmux_cmd new-session -d -s "$CHECKER_SESSION" "$checker_cmd '$RUNTIME_ROOT'"
  else
    tmux_cmd new-session -d -s "$CHECKER_SESSION" \
      "cd '$ROOT_DIR' && TRELLIS_TMUX_SOCKET='$TRELLIS_TMUX_SOCKET' ./scripts/trellis_checker_server.sh '$RUNTIME_ROOT'"
  fi
  local ready=0 i
  for ((i = 0; i < SOCKET_WAIT_SECS; i++)); do
    if [[ -S "$CHECKER_SOCKET" ]]; then ready=1; break; fi
    sleep 1
  done
  if [[ "$ready" -ne 1 ]]; then
    capture_pane_tail "$CHECKER_SESSION" "checker did not bind its socket"
    fail_phase launch_failed launch "checker server did not bind $CHECKER_SOCKET within ${SOCKET_WAIT_SECS}s"
  fi
  local run_cmd="${TRELLIS_CREATE_RUN_CMD:-}"
  local fp_mode="${TRELLIS_SOUNDNESS_FINGERPRINT_MODE:-v2_strict}"
  # Kernel binary: prefer a prebuilt RELEASE binary. scripts/trellis.sh falls
  # back to `cargo run` when TRELLIS_TRELLIS_KERNEL_CMD is unset, and that is
  # the DEBUG profile — an unoptimized binary running the whole step loop,
  # every boundary gate and the per-cycle state serialization for the life of
  # the run. Nothing in the create path ever set the variable, so every
  # browser-created run silently got the slow kernel while a hand-launched
  # run (which exports it) got the fast one.
  #
  # Honour an explicit value if the operator exported one, and fall back to
  # `cargo run` only when no release binary has been built — the same
  # behaviour as before, just no longer the default.
  local kernel_cmd="${TRELLIS_TRELLIS_KERNEL_CMD:-}"
  if [[ -z "$kernel_cmd" && -x "$ROOT_DIR/kernel/target/release/trellis_runtime_cli" ]]; then
    kernel_cmd="$ROOT_DIR/kernel/target/release/trellis_runtime_cli"
    echo "trellis-create: using the prebuilt release kernel ($kernel_cmd)"
  elif [[ -z "$kernel_cmd" ]]; then
    echo "trellis-create: WARNING — no release kernel at $ROOT_DIR/kernel/target/release/trellis_runtime_cli; the run will use a DEBUG 'cargo run' kernel, which is markedly slower. Build one with 'cargo build --release --manifest-path kernel/Cargo.toml'."
  fi
  if [[ -n "$run_cmd" ]]; then
    tmux_cmd new-session -d -s "$RUN_SESSION" \
      "TRELLIS_CHECKER_SOCKET='$CHECKER_SOCKET' $run_cmd '$RUNTIME_ROOT'"
  else
    tmux_cmd new-session -d -s "$RUN_SESSION" \
      "cd '$ROOT_DIR' && TRELLIS_TMUX_SOCKET='$TRELLIS_TMUX_SOCKET' TRELLIS_CHECKER_SOCKET='$CHECKER_SOCKET' TRELLIS_SOUNDNESS_FINGERPRINT_MODE='$fp_mode' TRELLIS_TRELLIS_KERNEL_CMD='$kernel_cmd' ./scripts/trellis.sh run '$RUNTIME_ROOT'"
  fi

  # --- closure sidecar ("grunts") -----------------------------------------
  # Writing `sidecar.enabled` into the config is NOT enough: the daemon is a
  # separate process in its own tmux session, launched by
  # scripts/trellis_sidecar.sh. Without this, the --grunts flag produced a
  # config the kernel honoured — it queued eligible nodes — while nothing
  # ever claimed them, so the viewer's grunts page showed a growing queue,
  # nothing in flight, and no attempts, forever. The toggle lied.
  #
  # Launch is conditional on the config the run actually got (the job-local
  # derived template is already merged into $REPO/trellis.config.json by
  # setup), not on the job's --grunts flag, so a template that enables the
  # sidecar without an explicit flag also gets its daemon.
  #
  # Best-effort by design: the daemon is inert-and-exits-0 when the block is
  # absent or disabled, and a sidecar that fails to come up must never fail a
  # run whose supervisor is healthy. It is reported, not fatal — the run works
  # without grunts, they are a speedup, not a correctness input.
  if python3 - "$REPO/trellis.config.json" <<'PY'
import json, sys
try:
    cfg = json.load(open(sys.argv[1], encoding="utf-8"))
except Exception:
    sys.exit(1)
sys.exit(0 if bool((cfg.get("sidecar") or {}).get("enabled")) else 1)
PY
  then
    local sidecar_cmd="${TRELLIS_CREATE_SIDECAR_CMD:-}"
    tmux_cmd kill-session -t "$SIDECAR_SESSION" 2>/dev/null || true
    if [[ -n "$sidecar_cmd" ]]; then
      tmux_cmd new-session -d -s "$SIDECAR_SESSION" \
        "$sidecar_cmd '$RUNTIME_ROOT' --repo '$REPO'"
    else
      tmux_cmd new-session -d -s "$SIDECAR_SESSION" \
        "cd '$ROOT_DIR' && TRELLIS_TMUX_SOCKET='$TRELLIS_TMUX_SOCKET' ./scripts/trellis_sidecar.sh '$RUNTIME_ROOT' --repo '$REPO'"
    fi
    echo "trellis-create: closure sidecar launched (session $SIDECAR_SESSION)"
  fi

  # --- verify (matrix row 20): a supervisor that exits immediately must not
  # count as a created run. `trellis_pause.sh status` is the authority on
  # "is the supervisor up"; protocol_state.json proves the kernel came up.
  write_status verifying verify
  local pause_cmd="${TRELLIS_CREATE_PAUSE_CMD:-bash $ROOT_DIR/scripts/trellis_pause.sh}"
  local verified=0 pstate
  for ((i = 0; i < VERIFY_SECS; i += 2)); do
    if [[ -f "$RUNTIME_ROOT/protocol_state.json" ]]; then
      pstate="$($pause_cmd status "$RUNTIME_ROOT" "$REPO" 2>/dev/null \
        | python3 -c 'import json,sys
try:
    print(json.load(sys.stdin).get("state", ""))
except Exception:
    print("")' || true)"
      if [[ "$pstate" == "running" ]]; then verified=1; break; fi
    fi
    sleep 2
  done
  if [[ "$verified" -ne 1 ]]; then
    capture_pane_tail "$RUN_SESSION" "supervisor did not stay up"
    fail_phase launch_failed verify "supervisor did not reach 'running' with protocol_state.json within ${VERIFY_SECS}s"
  fi

  # Last acts of launch: drop the marker (the repo is a run now), and adopt
  # the operator convenience symlink ONLY when nothing holds it — it is never
  # stolen from a live run (plan §6).
  rm -f "$MARKER"
  if [[ ! -e "$PROJECTS_ROOT/current" ]]; then
    ln -sfn "$REPO" "$PROJECTS_ROOT/current" 2>/dev/null || true
  fi

  stop_heartbeat
  write_status done done
  echo "trellis-create: done — $slug is live (run session $RUN_SESSION, checker $CHECKER_SESSION)."
}

# ===========================================================================
# Actions
# ===========================================================================
case "$action" in

  start)
    parse_flags "$@"
    [[ -n "$PAPER_ARG" ]] || die "start requires --paper <tex>"
    [[ -f "$PAPER_ARG" ]] || die "paper not found: $PAPER_ARG"
    case "$LOOGLE_ARG" in
      on|off) ;;
      *) die "start requires --loogle on|off" ;;
    esac
    if [[ -n "$TEMPLATE_ARG" ]]; then
      [[ -f "$TEMPLATE_ARG" ]] || die "config template not found: $TEMPLATE_ARG"
    fi
    validate_overrides
    for spec in ${REFERENCE_SPECS[@]+"${REFERENCE_SPECS[@]}"}; do
      [[ "$spec" == ?*=?* ]] || die "--reference expects <id>=<file>[:<source_id>], got: $spec"
    done
    [[ -e "$REPO" ]] && die "a project already exists at $REPO — pick another slug, or delete the failed job that holds it"

    # The atomic slug claim (rows 4-5): plain mkdir, no -p on the leaf.
    mkdir -p "$JOBS_ROOT"
    if ! mkdir "$JOB_DIR" 2>/dev/null; then
      die "a create job for '$slug' already exists at $JOB_DIR"
    fi

    transcoded="$(ingest_paper "$PAPER_ARG" "$JOB_DIR/paper.tex")" \
      || { rm -rf "$JOB_DIR"; die "paper is not decodable text: $PAPER_ARG"; }
    [[ "$transcoded" == "utf-8" ]] && transcoded=""

    # References are copied into the job dir now so phase B never depends on
    # the caller's paths surviving the wait at awaiting_targets.
    if ! ingest_reference_specs; then
      rm -rf "$JOB_DIR"
      exit 2
    fi

    J_INIT=1 J_SLUG="$slug" J_PAPER="$JOB_DIR/paper.tex" \
      J_PAPER_NAME="$(basename "$PAPER_ARG")" J_TRANSCODED="$transcoded" \
      J_LOOGLE="$LOOGLE_ARG" J_TEMPLATE="$TEMPLATE_ARG" \
      J_REMOTE_URL="$REMOTE_URL_ARG" J_MODEL="$MODEL_ARG" \
      J_EFFORT="$EFFORT_ARG" J_ROLE_OVERRIDES="$(role_overrides_json)" \
      J_GRUNTS="$GRUNTS_ARG" \
      J_GRUNT_WALL="$GRUNT_WALL_ARG" \
      J_ENV_MAP="$(env_map_json)" J_MAIN_RESULT_ENVS="$MAIN_RESULT_ENVS_ARG" \
      J_REFERENCES="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' \
        ${REF_SPECS_REWRITTEN[@]+"${REF_SPECS_REWRITTEN[@]}"})" \
      J_REPO="$REPO" J_RUNTIME_ROOT="$RUNTIME_ROOT" \
      job_update
    write_status intake intake
    spawn_phase phase-a
    echo "trellis-create: started — job $JOB_DIR, phase A in tmux session $SESSION"
    ;;

  resolve)
    require_job
    refuse_if_phase_running
    current="$(state_now)"
    case "$current" in
      awaiting_targets) ;;
      *)
        if ! state_is_failed "$current" && ! state_is_running "$current"; then
          die "resolve is legal in awaiting_targets or a failed state; '$slug' is '$current'"
        fi
        # A running state with the session gone is `interrupted`; re-resolving
        # it is legal (it relaunches phase A with the new choices).
        ;;
    esac
    parse_flags "$@"
    transcoded_update=""
    if [[ -n "$PAPER_ARG" ]]; then
      [[ -f "$PAPER_ARG" ]] || die "paper not found: $PAPER_ARG"
      transcoded_update="$(ingest_paper "$PAPER_ARG" "$JOB_DIR/paper.tex")" \
        || die "paper is not decodable text: $PAPER_ARG"
      [[ "$transcoded_update" == "utf-8" ]] && transcoded_update=""
      J_TRANSCODED="$transcoded_update" job_update
    fi
    # References are editable until confirm: --reference specs REPLACE the
    # recorded set wholesale (--clear-references empties it). This is row 26's
    # real recovery path — when a build fails on the immutability rule, the
    # fix is a NEW id for the changed file, re-resolve, re-confirm.
    if [[ "$CLEAR_REFERENCES" -eq 1 && ${#REFERENCE_SPECS[@]} -gt 0 ]]; then
      die "--clear-references and --reference are mutually exclusive"
    fi
    if [[ "$CLEAR_REFERENCES" -eq 1 ]]; then
      REFERENCE_SPECS=()
      ingest_reference_specs || exit 2   # prunes refs/ and yields the empty set
      J_REFERENCES="[]" job_update
    elif [[ ${#REFERENCE_SPECS[@]} -gt 0 ]]; then
      ingest_reference_specs || exit 2
      J_REFERENCES="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' \
        ${REF_SPECS_REWRITTEN[@]+"${REF_SPECS_REWRITTEN[@]}"})" job_update
    fi
    # Candidate keys can shift under a new paper or env set, so a previous
    # selection never survives a re-resolve (row 10: re-resolve -> re-confirm).
    if [[ ${#ENV_MAP_SPECS[@]} -gt 0 ]]; then
      J_ENV_MAP="$(env_map_json)" J_CLEAR_SELECTION=1 job_update
    else
      J_CLEAR_SELECTION=1 job_update
    fi
    if [[ "$MAIN_RESULT_ENVS_SET" -eq 1 ]]; then
      J_MAIN_RESULT_ENVS="$MAIN_RESULT_ENVS_ARG" job_update
    fi
    write_status intake intake
    spawn_phase phase-a
    echo "trellis-create: re-resolving — phase A in tmux session $SESSION"
    ;;

  confirm)
    require_job
    refuse_if_phase_running
    current="$(state_now)"
    [[ "$current" == "awaiting_targets" ]] \
      || die "confirm is only legal at awaiting_targets; '$slug' is '$current'"
    parse_flags "$@"
    [[ ${#SELECT_KEYS[@]} -gt 0 ]] || die "confirm requires at least one --select <candidate-key> (a run needs at least one target)"
    [[ -f "$RESOLUTION_JSON" ]] || die "no targets_resolution.json — run resolve first"
    # Validate the selection against the CURRENT resolution before anything
    # is recorded: every key must name a candidate.
    SELECT_JSON="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "${SELECT_KEYS[@]}")" \
    python3 - "$RESOLUTION_JSON" <<'PY' || exit 2
import json, os, sys
resolution = json.load(open(sys.argv[1]))
keys = {c.get("key") for c in resolution.get("candidates") or []}
selected = json.loads(os.environ["SELECT_JSON"])
unknown = [k for k in selected if k not in keys]
if unknown:
    sys.stderr.write(
        "trellis-create: selection names keys that are not candidates: "
        + ", ".join(unknown)
        + "\n  (candidates: "
        + (", ".join(sorted(k for k in keys if k)) or "none")
        + ")\n"
    )
    raise SystemExit(1)
PY
    J_SELECTED="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "${SELECT_KEYS[@]}")" \
      job_update
    write_status building setup
    spawn_phase phase-b
    echo "trellis-create: confirmed ${#SELECT_KEYS[@]} target(s) — phase B in tmux session $SESSION"
    ;;

  retry|resume)
    require_job
    refuse_if_phase_running
    current="$(state_now)"
    phase="$(json_field "$STATUS_JSON" phase)"
    case "$current" in
      done) die "'$slug' is done; nothing to retry" ;;
      awaiting_targets) die "'$slug' is awaiting target selection — confirm a selection (or resolve again); there is no phase to retry" ;;
      resolve_failed) spawn_target=phase-a ;;
      resolve_stale)
        # Row 10's recovery is re-resolve -> re-confirm. Re-running phase B
        # would only hit the same stale selection, so retry here IS a
        # re-resolve: fresh phase A, selection cleared, park for a new
        # confirm.
        spawn_target=phase-a
        J_CLEAR_SELECTION=1 job_update
        ;;
      build_failed|init_failed|launch_failed) spawn_target=phase-b ;;
      intake|resolving) spawn_target=phase-a ;;
      building|initing|launching|verifying) spawn_target=phase-b ;;
      *)
        # No status at all (e.g. claimed dir, crash before first write):
        # phase A is always safe to (re)run from intake.
        spawn_target="phase-${phase:-a}"
        [[ "$spawn_target" == "phase-a" || "$spawn_target" == "phase-b" ]] || spawn_target=phase-a
        ;;
    esac
    if [[ "$spawn_target" == "phase-b" ]]; then
      selected="$(json_field "$JOB_JSON" selected)"
      [[ -n "$selected" && "$selected" != "null" ]] \
        || die "cannot retry the build: job.json holds no confirmed selection (re-resolve and confirm)"
    fi
    write_status "$([[ "$spawn_target" == phase-a ]] && echo intake || echo building)" \
      "$([[ "$spawn_target" == phase-a ]] && echo intake || echo setup)"
    spawn_phase "$spawn_target"
    echo "trellis-create: retrying — $spawn_target in tmux session $SESSION"
    ;;

  status)
    # JSON to stdout. `effective_state` folds in the liveness the design's
    # heartbeat rule defines: a running state whose heartbeat is stale beyond
    # $TRELLIS_CREATE_STALE_SECS with the tmux session absent is
    # `interrupted` (rows 22-23). awaiting_targets has no process by design
    # and never decays (row 24).
    alive=0
    session_alive "$SESSION" && alive=1
    ALIVE="$alive" STALE_SECS="$STALE_SECS" \
      REPO="$REPO" RUNTIME_ROOT="$RUNTIME_ROOT" MARKER="$MARKER" \
      SESSION="$SESSION" SLUG="$slug" \
      python3 - "$JOB_DIR" <<'PY'
import json, os, sys, time
from pathlib import Path
job_dir = Path(sys.argv[1])

def load(name):
    try:
        return json.loads((job_dir / name).read_text(encoding="utf-8"))
    except Exception:
        return None

status = load("status.json") or {}
job = load("job.json") or {}
running = {"intake", "resolving", "building", "initing", "launching", "verifying"}
state = status.get("state") or ("missing" if not job_dir.is_dir() else "unknown")
alive = os.environ["ALIVE"] == "1"
stale_secs = int(os.environ["STALE_SECS"])
updated = status.get("updated_ts") or 0
age = int(time.time()) - int(updated) if updated else None
effective = state
if state in running and not alive and (age is None or age > stale_secs):
    effective = "interrupted"
print(json.dumps({
    "slug": os.environ["SLUG"],
    "state": state,
    "effective_state": effective,
    "stage": status.get("stage"),
    "phase": status.get("phase"),
    "started_ts": status.get("started_ts"),
    "updated_ts": status.get("updated_ts"),
    "heartbeat_age_secs": age,
    "error": status.get("error"),
    "tmux_session": os.environ["SESSION"],
    "tmux_alive": alive,
    "selected": job.get("selected"),
    "references": job.get("references"),
    "has_resolution": (job_dir / "targets_resolution.json").is_file(),
    "has_prev_resolution": (job_dir / "targets_resolution.prev.json").is_file(),
    "job_dir": str(job_dir),
    "repo": os.environ["REPO"],
    "repo_exists": Path(os.environ["REPO"]).exists(),
    "marker_present": Path(os.environ["MARKER"]).exists(),
    "runtime_root": os.environ["RUNTIME_ROOT"],
    "runtime_exists": Path(os.environ["RUNTIME_ROOT"]).exists(),
}, indent=2))
PY
    ;;

  delete)
    parse_flags "$@"
    [[ -d "$JOB_DIR" || -e "$MARKER" ]] \
      || die "nothing to delete for '$slug' (no job dir, no .trellis-creating marker)"
    if session_alive "$SESSION"; then
      die "a phase is still running for '$slug' (tmux session $SESSION) — wait for it or kill the session first"
    fi
    current="$(state_now)"
    [[ "$current" == "done" ]] \
      && die "'$slug' completed; its repo is a run now. Delete refuses — remove the job record by hand if you must."
    # A repo is only ever deleted while it bears the marker; a repo that lost
    # it has graduated (or predates this job) and is out of bounds.
    doomed=("$JOB_DIR")
    if [[ -e "$REPO" ]]; then
      [[ -e "$MARKER" ]] \
        || die "$REPO exists but bears no .trellis-creating marker — it is not this job's to delete"
      doomed+=("$REPO")
      [[ -e "$RUNTIME_ROOT" ]] && doomed+=("$RUNTIME_ROOT")
    elif [[ -e "$RUNTIME_ROOT" ]]; then
      die "$RUNTIME_ROOT exists with no repo beside it — refusing to guess; remove it by hand"
    fi
    [[ -d "$JOB_DIR" ]] || doomed=("${doomed[@]:1}")

    # The explicit confirmation NAMES THE PATHS (design §4): every doomed
    # path must be repeated back via --confirm, exactly and exhaustively.
    CONFIRM_JSON="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' \
      ${CONFIRM_PATHS[@]+"${CONFIRM_PATHS[@]}"})"
    DOOMED_JSON="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "${doomed[@]}")"
    if ! CONFIRM_JSON="$CONFIRM_JSON" DOOMED_JSON="$DOOMED_JSON" python3 - <<'PY'
import json, os, sys
confirmed = set(json.loads(os.environ["CONFIRM_JSON"]))
doomed = set(json.loads(os.environ["DOOMED_JSON"]))
if confirmed == doomed:
    sys.exit(0)
print(json.dumps({
    "error": "confirmation_required",
    "detail": "delete removes these paths; repeat each back with --confirm to proceed",
    "paths": sorted(doomed),
}, indent=2))
sys.exit(1)
PY
    then
      exit 3
    fi
    tmux_cmd kill-session -t "$CHECKER_SESSION" 2>/dev/null || true
    tmux_cmd kill-session -t "$RUN_SESSION" 2>/dev/null || true
    # Delete removes the runtime root the sidecar is pointed at; leaving its
    # daemon alive would leave it polling a directory that no longer exists.
    tmux_cmd kill-session -t "$SIDECAR_SESSION" 2>/dev/null || true
    for path in "${doomed[@]}"; do
      rm -rf "$path"
      echo "trellis-create: removed $path"
    done
    ;;

  phase-a)
    require_job
    run_phase_a
    ;;

  phase-b)
    require_job
    run_phase_b
    ;;

  *)
    die "unknown action: $action"
    ;;
esac
