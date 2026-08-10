#!/usr/bin/env bash
# Pause and resume a trellis run as a single, durable, reason-carrying state.
#
# WHY THIS EXISTS
#
# A run could already stop three unrelated ways, and only one of them was
# resumable without archaeology:
#
#   * a HumanGate (advance, need_input) parks the supervisor in a 1s poll
#     loop with no timeout. It resumes from the viewer, but the pause is
#     only as durable as the process: left overnight it is exposed to OOM,
#     tmux teardown, and cross-run session sweeps, and when it dies the
#     operator finds a dead run and no explanation.
#   * a halt marker records its reason on disk but needs manual triage.
#   * the `.trellis-stop-after-checkpoint` sentinel stops the run *cleanly*
#     — and is the worst of the three to come back from, because the kernel
#     removes the sentinel when it fires (runtime_cli.rs, "stop-after-
#     checkpoint sentinel detected"). Callers such as the bridge circuit
#     breaker write their reason INTO the sentinel body, so the stop
#     mechanism destroys the only record of why the run stopped.
#
# The fix is to stop treating the sentinel as the state. It is a TRIGGER —
# fire-once, self-consuming, and correctly so. The state lives in
# `<runtime>/pause_request.json`, which this script writes alongside the
# sentinel and which nothing deletes until a resume succeeds.
#
# That inverts the durability problem. A pause held open by a live process
# is only as durable as that process's environment; a pause held in on-disk
# state with the process DOWN cannot decay any further. Long pauses should
# therefore be process-down pauses.
#
# Resume replays `<runtime>/launch_env.json` (written by trellis.sh at every
# start) rather than guessing an environment, because guessing has a known
# cost: a restart missing TRELLIS_CSC_LAST_CLEAN_THRESHOLD once triggered a
# 226-cycle forced rewind.
#
# Usage:
#   trellis_pause.sh arm     <runtime_root> <repo_path> [--reason TEXT] [--by WHO] [--kind KIND]
#   trellis_pause.sh disarm  <runtime_root> <repo_path>
#   trellis_pause.sh status  <runtime_root> <repo_path>      # JSON to stdout
#   trellis_pause.sh resume  <runtime_root> <repo_path> [--force]
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TRELLIS_SH="$ROOT_DIR/scripts/trellis.sh"

die() { echo "trellis-pause: $*" >&2; exit 2; }

# Is the supervisor wrapper for this runtime alive?
#
# `pgrep -f` is ERE, so an alternation written `A\|B` matches nothing at all
# — that bug once produced a liveness probe that could never fire and a
# duplicate-launched supervisor. Keep this a single fixed pattern, and drop
# our own PID in case this script's argv ever contains the needle.
wrapper_pid() {
  local runtime_root="$1"
  pgrep -f "bash .*trellis\.sh run $runtime_root" 2>/dev/null \
    | grep -v "^$$\$" \
    | head -1 || true
}

json_field() {
  python3 -c "
import json,sys
try:
    d=json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)
v=d
for k in sys.argv[2].split('.'):
    if not isinstance(v,dict): sys.exit(0)
    v=v.get(k)
    if v is None: sys.exit(0)
sys.stdout.write(str(v))
" "$1" "$2" 2>/dev/null || true
}

action="${1:-}"
runtime_root="${2:-}"
repo_path="${3:-}"
shift 3 2>/dev/null || true

[[ -n "$action" && -n "$runtime_root" && -n "$repo_path" ]] \
  || die "usage: trellis_pause.sh <arm|disarm|status|resume> <runtime_root> <repo_path> [opts]"
[[ -d "$runtime_root" ]] || die "runtime root is not a directory: $runtime_root"

REQUEST="$runtime_root/pause_request.json"
LAUNCH_ENV="$runtime_root/launch_env.json"
SENTINEL="$repo_path/.trellis-stop-after-checkpoint"

reason=""; by="operator"; kind="manual"; force=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --reason) reason="${2:-}"; shift 2 ;;
    --by)     by="${2:-}";     shift 2 ;;
    --kind)   kind="${2:-}";   shift 2 ;;
    --force)  force=1;         shift ;;
    *) die "unknown option: $1" ;;
  esac
done

case "$action" in

  arm)
    [[ -d "$repo_path" ]] || die "repo path is not a directory: $repo_path"
    [[ -n "$reason" ]] || reason="pause requested by $by"
    cycle="$(json_field "$runtime_root/protocol_state.json" cycle)"
    REASON="$reason" BY="$by" KIND="$kind" CYCLE="$cycle" \
      python3 - "$REQUEST" <<'PY'
import json, os, sys, time
from pathlib import Path
target = Path(sys.argv[1])
payload = {
    "armed_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    "armed_at_epoch": int(time.time()),
    "armed_by": os.environ.get("BY") or "operator",
    "kind": os.environ.get("KIND") or "manual",
    "reason": os.environ.get("REASON") or "",
    "armed_at_cycle": int(os.environ["CYCLE"]) if (os.environ.get("CYCLE") or "").isdigit() else None,
}
tmp = target.with_suffix(".json.tmp")
tmp.write_text(json.dumps(payload, indent=2, sort_keys=True), encoding="utf-8")
tmp.replace(target)
PY
    # The sentinel body is what the kernel's own log and any pre-existing
    # tooling surface; the durable copy is pause_request.json above.
    printf '%s\n' "[trellis-pause] $kind: $reason (armed by $by)" > "$SENTINEL"
    echo "trellis-pause: armed — run will stop cleanly at the next checkpoint."
    echo "trellis-pause:   reason: $reason"
    ;;

  disarm)
    rm -f "$SENTINEL" "$REQUEST"
    echo "trellis-pause: disarmed (sentinel and pause request cleared)."
    ;;

  status)
    pid="$(wrapper_pid "$runtime_root")"
    RUNTIME="$runtime_root" PID="$pid" \
    SENTINEL_PRESENT="$([[ -e "$SENTINEL" ]] && echo 1 || echo 0)" \
      python3 - "$REQUEST" "$LAUNCH_ENV" <<'PY'
import json, os, sys
from pathlib import Path

def load(p):
    try:
        return json.loads(Path(p).read_text())
    except Exception:
        return None

request = load(sys.argv[1])
launch = load(sys.argv[2])
pid = (os.environ.get("PID") or "").strip()
alive = bool(pid)
armed = os.environ.get("SENTINEL_PRESENT") == "1"

# Four states, distinguished by (process alive?, pause requested?):
#   running  — alive, nothing requested
#   arming   — alive, request on disk; will stop at the next checkpoint
#   paused   — down, request on disk; stopped on purpose, resumable
#   down     — down, no request; stopped for a reason nobody recorded
if alive and not request:
    state = "running"
elif alive:
    state = "arming"
elif request:
    state = "paused"
else:
    state = "down"

print(json.dumps({
    "state": state,
    "wrapper_pid": int(pid) if pid.isdigit() else None,
    "sentinel_present": armed,
    "request": request,
    "resumable": state in ("paused", "down") and launch is not None,
    "launch_env": {
        "present": launch is not None,
        "captured_at": (launch or {}).get("captured_at"),
        "trellis_head": (launch or {}).get("trellis_head"),
        "tmux_session": (launch or {}).get("tmux_session"),
    },
}, indent=2))
PY
    ;;

  resume)
    pid="$(wrapper_pid "$runtime_root")"
    if [[ -n "$pid" ]]; then
      die "supervisor is already running (pid $pid) — nothing to resume."
    fi
    [[ -f "$LAUNCH_ENV" ]] \
      || die "no launch_env.json in $runtime_root; this run predates env capture. Restart by hand, per REWINDING.md."

    # Refuse to resume onto a moved source tree unless forced. Coming up on
    # a diverged HEAD makes the startup fingerprint-observe return empty,
    # which the kernel reads as state-vs-disk divergence and answers with an
    # auto-rewind — the failure is loud, destructive, and entirely avoidable
    # here. The documented path in that case is a full REWINDING.md resume
    # (prep -> checker -> prep), which prewarms what the observe needs.
    # Compare against the tree the RUN was launched from, not the tree this
    # script happens to live in. Runs are routinely launched from a git
    # worktree with its own HEAD and its own kernel binary, so checking
    # $ROOT_DIR would compare two unrelated trees and refuse every resume.
    recorded_head="$(json_field "$LAUNCH_ENV" trellis_head)"
    recorded_root="$(json_field "$LAUNCH_ENV" trellis_root)"
    [[ -n "$recorded_root" ]] || recorded_root="$ROOT_DIR"
    current_head="$(git -C "$recorded_root" rev-parse HEAD 2>/dev/null || true)"
    if [[ -n "$recorded_head" && -n "$current_head" && "$recorded_head" != "$current_head" ]]; then
      if [[ "$force" -ne 1 ]]; then
        echo "trellis-pause: REFUSING to resume — the trellis source tree has moved." >&2
        echo "trellis-pause:   source tree: $recorded_root" >&2
        echo "trellis-pause:   launched at: $recorded_head" >&2
        echo "trellis-pause:   now at:      $current_head" >&2
        echo "trellis-pause: a plain restart on a diverged HEAD can trigger an auto-rewind." >&2
        echo "trellis-pause: follow REWINDING.md (prep -> checker -> prep), or pass --force." >&2
        exit 3
      fi
      echo "trellis-pause: WARNING: resuming across a diverged HEAD because --force was given." >&2
    fi

    # Materialize the recorded environment as a launcher rather than
    # interpolating it into a tmux send-keys string, so values containing
    # spaces or quotes cannot reshape the command.
    LAUNCHER="$runtime_root/resume_launch.sh"
    TRELLIS_SH="$TRELLIS_SH" python3 - "$LAUNCH_ENV" "$LAUNCHER" "$runtime_root" <<'PY'
import json, os, shlex, sys
from pathlib import Path
launch = json.loads(Path(sys.argv[1]).read_text())
launcher, runtime_root = Path(sys.argv[2]), sys.argv[3]
lines = ["#!/usr/bin/env bash", "# Generated by trellis_pause.sh resume. Replays the recorded launch env.", "set -euo pipefail", ""]
for name, value in sorted((launch.get("env") or {}).items()):
    lines.append(f"export {name}={shlex.quote(str(value))}")
cwd = launch.get("cwd") or launch.get("trellis_root") or "."
lines += ["", f"cd {shlex.quote(cwd)}", ""]
# The recorded trellis.sh, not this script's sibling: a run launched from a
# worktree must come back up on that worktree's script and binary, or it
# resumes against a different kernel than the one that wrote its state.
trellis_sh = launch.get("trellis_sh") or os.environ["TRELLIS_SH"]
cmd = ["bash", trellis_sh, "run", runtime_root]
if launch.get("max_steps"):
    cmd.append(str(launch["max_steps"]))
lines.append("exec " + " ".join(shlex.quote(c) for c in cmd))
launcher.write_text("\n".join(lines) + "\n", encoding="utf-8")
launcher.chmod(0o755)
PY

    # Clear the trigger before relaunching. A sentinel left on disk is
    # consumed by the very first step-loop iteration, so the run would come
    # up and immediately stop again — the committed-sentinel halt loop.
    rm -f "$SENTINEL"

    session="$(json_field "$LAUNCH_ENV" tmux_session)"
    [[ -n "$session" ]] || session="trellis-run-$(basename "$repo_path")"
    if ! tmux -L trellis has-session -t "$session" 2>/dev/null; then
      tmux -L trellis new-session -d -s "$session" -c "$ROOT_DIR"
    fi
    tmux -L trellis send-keys -t "$session" "bash $LAUNCHER" Enter
    echo "trellis-pause: resume sent to tmux session $session"

    # Only clear the pause record once the run is actually back. If the
    # relaunch fails, the record must survive so the viewer still explains
    # why the run is down and still offers a resume.
    for _ in $(seq 1 12); do
      sleep 1
      pid="$(wrapper_pid "$runtime_root")"
      [[ -n "$pid" ]] && break
    done
    if [[ -n "$pid" ]]; then
      rm -f "$REQUEST"
      echo "trellis-pause: resumed — wrapper pid $pid"
    else
      echo "trellis-pause: WARNING: no wrapper detected 12s after relaunch; pause record kept." >&2
      echo "trellis-pause: inspect: tmux -L trellis attach -t $session" >&2
      exit 4
    fi
    ;;

  *)
    die "unknown action: $action"
    ;;
esac
