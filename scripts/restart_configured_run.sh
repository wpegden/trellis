#!/usr/bin/env bash
# Unified-checker RPC mode (design plan §4):
#
# This script always starts a sibling tmux session
# `trellis-checker-<slug>` running `scripts/trellis_checker_server.sh
# <runtime_root>`, waits for the server to bind its UNIX socket at
# `<runtime_root>/sockets/checker.sock`, and sets
# TRELLIS_CHECKER_SOCKET=<runtime_root>/sockets/checker.sock in the
# supervisor's environment. Worker bursts and supervisor-side observations
# route compile_node and the remaining acceptance ops through the unified
# checker — getting the prepare-cache / lock-split / olean-prewarm wins
# automatically. The checker server is the only supported way to run
# authoritative acceptance lake checks; there is no host-lake fallback.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BURST_USER="${BURST_USER:-$(id -un)}"

TRELLIS_TMUX_SOCKET="${TRELLIS_TMUX_SOCKET:-trellis}"
export TRELLIS_TMUX_SOCKET
tmux_cmd() { tmux -L "$TRELLIS_TMUX_SOCKET" "$@"; }

usage() {
  cat <<'EOF'
Usage:
  scripts/restart_configured_run.sh [--no-run] [--no-current]
                                    [--check-only] <config_path> <runtime_root>

This performs a clean restart of an existing configured run:
  - copies the current config/policy/paper to a temporary template area
  - fully recreates the repo with `setup_repo.sh --reset`
  - reinitializes the runtime root with `trellis.sh init`
  - refreshes the `$HOME/math/current` and `$HOME/trellis-web/current` aliases
  - restarts the viewer server
  - optionally launches the runtime in tmux

Flags:
  --no-run            Set up the workspace but skip launching the supervisor.
  --no-current        Skip refreshing $HOME/math/current and the
                      trellis-web symlinks.
  --check-only        Dry-run: print the planned tmux invocations and exit
                      without touching the filesystem. Useful for review.

It is intended to replace ad hoc manual restarts.
EOF
}

RUN_AFTER=1
UPDATE_CURRENT=1
CHECK_ONLY=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-run)
      RUN_AFTER=0
      shift
      ;;
    --no-current)
      UPDATE_CURRENT=0
      shift
      ;;
    --check-only)
      CHECK_ONLY=1
      shift
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
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      break
      ;;
  esac
done

CONFIG_PATH="${1:-}"
RUNTIME_ROOT="${2:-}"
if [[ -z "$CONFIG_PATH" || -z "$RUNTIME_ROOT" ]]; then
  usage >&2
  exit 2
fi

CONFIG_PATH="$(python3 - "$CONFIG_PATH" <<'PY'
from pathlib import Path
import sys
print(Path(sys.argv[1]).resolve())
PY
)"
RUNTIME_ROOT="$(python3 - "$RUNTIME_ROOT" <<'PY'
from pathlib import Path
import sys
print(Path(sys.argv[1]).resolve())
PY
)"

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

eval "$(
python3 - "$CONFIG_PATH" "$TMP_DIR" "$RUNTIME_ROOT" <<'PY'
import json
import shutil
import sys
from pathlib import Path

config_path = Path(sys.argv[1]).resolve()
tmp_dir = Path(sys.argv[2]).resolve()
runtime_root = Path(sys.argv[3]).resolve()
raw = json.loads(config_path.read_text(encoding="utf-8"))

repo_path = Path(str(raw["repo_path"])).resolve()
slug = repo_path.name

policy_raw = str(raw.get("policy_path", "") or "").strip()
if policy_raw:
    policy_path = Path(policy_raw)
    if not policy_path.is_absolute():
        policy_path = (repo_path / policy_path).resolve()
else:
    policy_path = repo_path / "trellis.policy.json"

paper_dir = repo_path / "paper"
paper_candidates = sorted(paper_dir.glob("*.tex"))
if not paper_candidates:
    raise SystemExit(f"no paper .tex found under {paper_dir}")
paper_path = paper_candidates[0].resolve()

template_config = tmp_dir / "trellis.template.config.json"
template_policy = tmp_dir / "trellis.template.policy.json"
paper_copy = tmp_dir / paper_path.name
shutil.copyfile(config_path, template_config)
if policy_path.exists():
    shutil.copyfile(policy_path, template_policy)
else:
    template_policy.write_text("{}\n", encoding="utf-8")
shutil.copyfile(paper_path, paper_copy)

targets = raw.get("workflow", {}).get("main_result_targets", []) or []
labels = []
all_labeled = True
for target in targets:
    if not isinstance(target, dict):
        all_labeled = False
        break
    label = str(target.get("tex_label", "") or "").strip()
    if not label:
        all_labeled = False
        break
    labels.append(label)
main_result_labels = ",".join(labels) if all_labeled and labels else ""

# Preserve the existing config's Loogle setting across the setup --reset
# (setup_repo.sh now requires --loogle on|off and rewrites loogle.enabled).
# Absent key -> on, matching the supervisor default.
loogle_cfg = raw.get("loogle")
loogle_on = bool(loogle_cfg.get("enabled", True)) if isinstance(loogle_cfg, dict) else True

run_session = f"trellis-run-{slug}"
checker_session = f"trellis-checker-{slug}"
# Mirror server.py's _runtime_socket_path: <runtime_root>/sockets/checker.sock.
checker_socket = runtime_root / "sockets" / "checker.sock"
runtime_namespace_source = runtime_root.parent.name or runtime_root.name
runtime_namespace = "".join(
    ch if ch.isalnum() or ch in {"-", "_"} else "-"
    for ch in runtime_namespace_source.strip()
).strip("-_") or "runtime"
runtime_namespace = runtime_namespace[:48]

print(f"REPO_PATH={repo_path!s}")
print(f"PROJECT_SLUG={slug}")
print(f"TEMPLATE_CONFIG={template_config!s}")
print(f"TEMPLATE_POLICY={template_policy!s}")
print(f"PAPER_COPY={paper_copy!s}")
print(f"MAIN_RESULT_LABELS={main_result_labels}")
print(f"RUN_SESSION={run_session}")
print(f"CHECKER_SESSION={checker_session}")
print(f"CHECKER_SOCKET_PATH={checker_socket!s}")
print(f"RUNTIME_NAMESPACE={runtime_namespace}")
print(f"LOOGLE_SETTING={'on' if loogle_on else 'off'}")
PY
)"

# --check-only: print the planned tmux invocations and exit without
# touching the filesystem. Useful for review before pulling the trigger,
# especially since RPC mode adds a sibling tmux session.
if [[ "$CHECK_ONLY" -eq 1 ]]; then
  cat <<EOF
plan: restart_configured_run.sh dry-run
  config_path=$CONFIG_PATH
  runtime_root=$RUNTIME_ROOT
  repo_path=$REPO_PATH
  project_slug=$PROJECT_SLUG
  tmux_socket=$TRELLIS_TMUX_SOCKET
  run_after=$RUN_AFTER
  update_current=$UPDATE_CURRENT
  kill_run_session=tmux -L $TRELLIS_TMUX_SOCKET kill-session -t $RUN_SESSION
  kill_checker_session=tmux -L $TRELLIS_TMUX_SOCKET kill-session -t $CHECKER_SESSION
  checker_session=$CHECKER_SESSION
  checker_socket=$CHECKER_SOCKET_PATH
  start_checker=tmux -L $TRELLIS_TMUX_SOCKET new-session -d -s $CHECKER_SESSION "$ROOT_DIR/scripts/trellis_checker_server.sh $RUNTIME_ROOT"
  supervisor_env=TRELLIS_CHECKER_SOCKET=$CHECKER_SOCKET_PATH
EOF
  if [[ "$RUN_AFTER" -eq 1 ]]; then
    cat <<EOF
  start_run=tmux -L $TRELLIS_TMUX_SOCKET new-session -d -s $RUN_SESSION "cd $ROOT_DIR && TRELLIS_TMUX_SOCKET=$TRELLIS_TMUX_SOCKET TRELLIS_CHECKER_SOCKET=$CHECKER_SOCKET_PATH ./scripts/trellis.sh run $RUNTIME_ROOT"
EOF
  fi
  exit 0
fi

tmux_cmd kill-session -t "$RUN_SESSION" >/dev/null 2>&1 || true
tmux_cmd kill-session -t "$CHECKER_SESSION" >/dev/null 2>&1 || true
python3 - "$RUN_SESSION" "$CHECKER_SESSION" "$RUNTIME_NAMESPACE" "$TRELLIS_TMUX_SOCKET" <<'PY'
import subprocess
import sys

run_session = sys.argv[1]
checker_session = sys.argv[2]
runtime_namespace = sys.argv[3]
socket = sys.argv[4]
tmux = ["tmux", "-L", socket]
targets = []
try:
    listing = subprocess.check_output([*tmux, "ls"], text=True, stderr=subprocess.DEVNULL)
except subprocess.CalledProcessError:
    listing = ""
for raw_line in listing.splitlines():
    name = raw_line.split(":", 1)[0].strip()
    if not name:
        continue
    if (
        name == run_session
        or name == checker_session
        or name.startswith(f"trellis-{runtime_namespace}-")
    ):
        targets.append(name)
for name in targets:
    subprocess.run([*tmux, "kill-session", "-t", name], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
PY
python3 - "$RUNTIME_ROOT" "$REPO_PATH" <<'PY'
import os
import signal
import subprocess
import sys

runtime_root = sys.argv[1]
repo_path = sys.argv[2]
current_pid = os.getpid()
parent_pid = os.getppid()
needles = (
    "trellis_runtime_cli",
    "trellis.sh run",
    ".trellis/scripts/check.py",
    "print-axioms",
    "lean-compile-node",
    "materialize-tablet-oleans",
    "prepare-compiled-support",
    " bwrap ",
    " claude ",
    " gemini ",
    " codex ",
)

ps = subprocess.check_output(["ps", "-eo", "pid=,args="], text=True)
for raw_line in ps.splitlines():
    line = raw_line.strip()
    if not line:
        continue
    pid_str, _, args = line.partition(" ")
    if not pid_str.isdigit():
        continue
    pid = int(pid_str)
    if pid in {current_pid, parent_pid}:
        continue
    if runtime_root not in args and repo_path not in args:
        continue
    if not any(needle in args for needle in needles):
        continue
    try:
        os.kill(pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        pass
PY
if sudo -n -u "$BURST_USER" true >/dev/null 2>&1; then
  sudo -n -u "$BURST_USER" python3 - "$RUNTIME_ROOT" "$REPO_PATH" <<'PY'
import os
import signal
import subprocess
import sys

runtime_root = sys.argv[1]
repo_path = sys.argv[2]
needles = (
    "trellis_runtime_cli",
    "trellis.sh run",
    ".trellis/scripts/check.py",
    "print-axioms",
    "lean-compile-node",
    "materialize-tablet-oleans",
    "prepare-compiled-support",
    " bwrap ",
    " claude ",
    " gemini ",
    " codex ",
)

ps = subprocess.check_output(["ps", "-eo", "pid=,args="], text=True)
for raw_line in ps.splitlines():
    line = raw_line.strip()
    if not line:
        continue
    pid_str, _, args = line.partition(" ")
    if not pid_str.isdigit():
        continue
    pid = int(pid_str)
    if runtime_root not in args and repo_path not in args:
        continue
    if not any(needle in args for needle in needles):
        continue
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
PY
fi
sleep 1
python3 - "$RUNTIME_ROOT" "$REPO_PATH" <<'PY'
import os
import signal
import subprocess
import sys
import time

runtime_root = sys.argv[1]
repo_path = sys.argv[2]
needles = (
    "trellis_runtime_cli",
    "trellis.sh run",
    ".trellis/scripts/check.py",
    "print-axioms",
    "lean-compile-node",
    "materialize-tablet-oleans",
    "prepare-compiled-support",
    " bwrap ",
    " claude ",
    " gemini ",
    " codex ",
)

def matching_processes() -> list[tuple[int, str]]:
    rows: list[tuple[int, str]] = []
    ps = subprocess.check_output(["ps", "-eo", "pid=,args="], text=True)
    for raw_line in ps.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        pid_str, _, args = line.partition(" ")
        if not pid_str.isdigit():
            continue
        pid = int(pid_str)
        if runtime_root not in args and repo_path not in args:
            continue
        if not any(needle in args for needle in needles):
            continue
        rows.append((pid, args))
    return rows

deadline = time.time() + 5.0
while time.time() < deadline:
    rows = matching_processes()
    if not rows:
        raise SystemExit(0)
    time.sleep(0.5)

for pid, _ in matching_processes():
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
time.sleep(0.5)
rows = matching_processes()
if rows:
    print("restart_configured_run.sh: failed to quiesce existing run processes:", file=sys.stderr)
    for pid, args in rows:
        print(f"  pid={pid} args={args}", file=sys.stderr)
    raise SystemExit(1)
PY

SETUP_ARGS=(--reset --yes --loogle "$LOOGLE_SETTING")
if [[ -n "$MAIN_RESULT_LABELS" ]]; then
  SETUP_ARGS+=(--main-result-labels "$MAIN_RESULT_LABELS")
fi

# Refresh the supervisor's agent CLIs. Operator-initiated path; mid-run
# updates are intentionally NOT done from the quota probe (it dismisses
# the codex update dialog instead) so this fresh-start step is the
# canonical place to pick up new releases. Skip if npm is missing or
# the install fails — neither should block the restart.
if command -v npm >/dev/null 2>&1; then
  for pkg in @openai/codex @anthropic-ai/claude-code @google/gemini-cli; do
    npm install -g "$pkg@latest" >/dev/null 2>&1 \
      && echo "  Updated $pkg" \
      || echo "  Skipped $pkg (npm install failed)"
  done
fi

CONFIG_TEMPLATE="$TEMPLATE_CONFIG" \
POLICY_TEMPLATE="$TEMPLATE_POLICY" \
"$ROOT_DIR/scripts/setup_repo.sh" "${SETUP_ARGS[@]}" "$REPO_PATH" "$PAPER_COPY" "$PROJECT_SLUG"

rm -rf "$RUNTIME_ROOT"
"$ROOT_DIR/scripts/trellis.sh" init "$REPO_PATH/trellis.config.json" "$RUNTIME_ROOT" >/dev/null

mkdir -p $HOME/math
if [[ "$UPDATE_CURRENT" -eq 1 ]]; then
  ln -sfn "$REPO_PATH" $HOME/math/current
  rm -rf $HOME/trellis-web/current
  mkdir -p $HOME/trellis-web/current
  ln -sfn "$REPO_PATH/.trellis/viewer" $HOME/trellis-web/current/api
  ln -sfn "$ROOT_DIR/viewer/public/index.html" $HOME/trellis-web/current/index.html
fi

# The viewer is optional (and its readiness probe can false-fail, e.g. with
# --no-current). Never let it gate the essential checker server + supervisor:
# treat a non-zero exit as a warning and continue.
"$ROOT_DIR/scripts/start_viewer.sh" >/dev/null \
  || echo "warning: viewer did not start (optional); continuing without it" >&2

# Unified-checker UNIX-socket server (design plan §4). This always launches
# a sibling tmux session that owns the supervisor-side lake dispatcher. The
# supervisor's run-session below picks up TRELLIS_CHECKER_SOCKET from its env
# so worker bursts and supervisor-side observations route acceptance through
# the server. The checker server is the only supported way to run
# authoritative acceptance lake checks; there is no host-lake fallback.
# Drop any stale socket node before starting. Normally `trellis.sh init`
# has already wiped the runtime root above, so the socket cannot exist
# yet — but a previous run that crashed could have left state under
# <runtime_root>/sockets that survived if init was skipped. The server
# itself also unlinks stale nodes during start(); doing it here makes the
# wait-for-ready loop below unambiguous (any socket we see is from THIS
# launch).
rm -f "$CHECKER_SOCKET_PATH"
tmux_cmd new-session -d -s "$CHECKER_SESSION" \
  "cd '$ROOT_DIR' && TRELLIS_TMUX_SOCKET='$TRELLIS_TMUX_SOCKET' ./scripts/trellis_checker_server.sh '$RUNTIME_ROOT'"

# Wait up to 30 s for the server to bind the socket. If it doesn't
# appear, fail loudly — launching the supervisor with
# TRELLIS_CHECKER_SOCKET set but no listener would make every worker
# burst fail with `supervisor_unavailable`.
CHECKER_READY=0
for _ in $(seq 1 30); do
  if [[ -S "$CHECKER_SOCKET_PATH" ]]; then
    CHECKER_READY=1
    break
  fi
  sleep 1
done
if [[ "$CHECKER_READY" -ne 1 ]]; then
  echo "restart_configured_run.sh: checker server did not bind socket within 30s" >&2
  echo "  expected: $CHECKER_SOCKET_PATH" >&2
  echo "  inspect: tmux -L $TRELLIS_TMUX_SOCKET attach -t $CHECKER_SESSION" >&2
  exit 1
fi
SUPERVISOR_CHECKER_ENV="TRELLIS_CHECKER_SOCKET='$CHECKER_SOCKET_PATH'"

if [[ "$RUN_AFTER" -eq 1 ]]; then
  tmux_cmd new-session -d -s "$RUN_SESSION" \
    "cd '$ROOT_DIR' && TRELLIS_TMUX_SOCKET='$TRELLIS_TMUX_SOCKET' $SUPERVISOR_CHECKER_ENV ./scripts/trellis.sh run '$RUNTIME_ROOT'"
fi

cat <<EOF
repo_path=$REPO_PATH
runtime_root=$RUNTIME_ROOT
project_slug=$PROJECT_SLUG
run_session=$RUN_SESSION
tmux_socket=$TRELLIS_TMUX_SOCKET
attach_run=tmux -L $TRELLIS_TMUX_SOCKET attach -t $RUN_SESSION
current_alias=$([[ "$UPDATE_CURRENT" -eq 1 ]] && echo $HOME/math/current || echo skipped)
launched=$([[ "$RUN_AFTER" -eq 1 ]] && echo yes || echo no)
checker_session=$CHECKER_SESSION
checker_socket=$CHECKER_SOCKET_PATH
attach_checker=tmux -L $TRELLIS_TMUX_SOCKET attach -t $CHECKER_SESSION
EOF
