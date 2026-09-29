#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VIEWER_DIR="$ROOT_DIR/viewer"
SESSION_NAME="${TRELLIS_VIEWER_SESSION:-trellis_viewer}"
PORT="${TRELLIS_VIEWER_PORT:-3301}"
BASE_PATH="${TRELLIS_VIEWER_BASE_PATH:-/trellis}"
PROMPTS_BASE="${TRELLIS_PROMPTS_BASE_PATH:-/prompts}"
PROJECTS_ROOT="${TRELLIS_PROJECTS_ROOT:-$HOME/math}"
STATIC_OUT="${TRELLIS_STATIC_OUT:-$HOME/trellis-web}"
LOG="${TRELLIS_VIEWER_LOG:-$HOME/.trellis-viewer.log}"

# The viewer is an operator control surface: it stops and relaunches
# supervisors and runs un-sandboxed as you, next to your provider
# credentials. Two knobs govern who can reach it.
#
#   TRELLIS_VIEWER_BIND     which interface to listen on. Loopback by
#                           default. Set it wider only when you mean to be
#                           reachable from the network; the viewer logs a
#                           warning naming what it bound and why that is a
#                           choice.
#   TRELLIS_VIEWER_CONTROL  1 (default) registers the mutating endpoints,
#                           each requiring a secret header. Set it to 0 to
#                           make the viewer read-only — DO THIS BEFORE
#                           EXPOSING IT PUBLICLY. The nginx installers
#                           additionally deny the whole control namespace.
#
# Two more govern who the viewer answers, both of which have working defaults
# and need setting only when the derived answer is wrong.
#
#   TRELLIS_VIEWER_ALLOWED_HOSTS
#                           extra names to accept in the Host header. The
#                           viewer already accepts localhost, any loopback
#                           literal, this machine's own short and fully
#                           qualified names, and whatever TRELLIS_VIEWER_BIND
#                           names — so an ordinary install needs nothing here.
#                           Set it for an alias or a Host-rewriting proxy;
#                           `*` turns the check off. A refused request says
#                           all of this in its response body.
#   TRELLIS_VIEWER_READ_TOKEN
#                           auto (default) requires the secret path segment on
#                           READS whenever the bind is not loopback, and never
#                           when it is. 1 always requires it, 0 never does.
#
# Controls live under a secret 8-character path segment: browsing
# <base>/<token>/ behaves exactly like <base>/ but with the controls enabled,
# and <base>/<token>/<project> likewise. The plain URLs keep working, without
# controls. The control URL is echoed below and lands in $LOG.
BIND="${TRELLIS_VIEWER_BIND:-127.0.0.1}"
CONTROL="${TRELLIS_VIEWER_CONTROL:-1}"
ALLOWED_HOSTS="${TRELLIS_VIEWER_ALLOWED_HOSTS:-}"
READ_TOKEN="${TRELLIS_VIEWER_READ_TOKEN:-auto}"

TRELLIS_TMUX_SOCKET="${TRELLIS_TMUX_SOCKET:-trellis}"
export TRELLIS_TMUX_SOCKET
tmux_cmd() { tmux -L "$TRELLIS_TMUX_SOCKET" "$@"; }

if [[ ! -d "$VIEWER_DIR" ]]; then
  echo "viewer directory missing: $VIEWER_DIR" >&2
  exit 1
fi

if [[ ! -d "$VIEWER_DIR/node_modules" ]]; then
  npm --prefix "$VIEWER_DIR" install >/dev/null
fi

for legacy_session in "$SESSION_NAME" trellis-viewer trellis_viewer; do
  tmux_cmd kill-session -t "$legacy_session" >/dev/null 2>&1 || true
done

python3 - "$PORT" <<'PY'
import os
import signal
import subprocess
import sys

port = sys.argv[1]
try:
    ps = subprocess.check_output(["ss", "-ltnp"], text=True, stderr=subprocess.DEVNULL)
except Exception:
    raise SystemExit(0)

for line in ps.splitlines():
    if f":{port} " not in line:
        continue
    for part in line.split("pid=")[1:]:
        pid_text = part.split(",", 1)[0].split(")", 1)[0]
        if not pid_text.isdigit():
            continue
        try:
            os.kill(int(pid_text), signal.SIGTERM)
        except ProcessLookupError:
            pass
PY
sleep 1

tmux_cmd kill-session -t "$SESSION_NAME" >/dev/null 2>&1 || true

# Run node under a restart loop with persistent logging. A bare `node` in tmux
# leaves no trace when it dies (the pane vanishes with the session), so crashes
# were undiagnosable and stayed down until a manual restart. The loop keeps the
# viewer self-healing and $LOG captures stdout/stderr — including a V8 fatal
# ("JavaScript heap out of memory") or the keep-alive handlers' stack traces —
# so the next crash is diagnosable. The 2s backoff avoids a tight spin if it
# fails immediately (e.g. a persistent port bind failure).
tmux_cmd new-session -d -s "$SESSION_NAME" \
  "cd '$VIEWER_DIR' && export PORT='$PORT' BASE_PATH='$BASE_PATH' PROMPTS_BASE='$PROMPTS_BASE' PROJECTS_ROOT='$PROJECTS_ROOT' STATIC_OUT='$STATIC_OUT' TRELLIS_TMUX_SOCKET='$TRELLIS_TMUX_SOCKET' TRELLIS_VIEWER_BIND='$BIND' TRELLIS_VIEWER_CONTROL='$CONTROL' TRELLIS_VIEWER_ALLOWED_HOSTS='$ALLOWED_HOSTS' TRELLIS_VIEWER_READ_TOKEN='$READ_TOKEN'; while true; do echo \"[\$(date '+%F %T')] starting viewer (pid \$\$)\" >>'$LOG'; node server.js >>'$LOG' 2>&1; echo \"[\$(date '+%F %T')] viewer exited rc=\$? — restarting in 2s\" >>'$LOG'; sleep 2; done"

# A wildcard bind is reachable on loopback; a specific non-loopback address
# is not, so probe whatever we actually asked it to listen on.
PROBE_HOST="$BIND"
if [[ "$BIND" == "0.0.0.0" || "$BIND" == "::" ]]; then
  PROBE_HOST="127.0.0.1"
fi

# Liveness only. This path is the one the viewer's read gate always lets
# through, so the probe still works when TRELLIS_VIEWER_READ_TOKEN gates
# reads behind the control-token URL (it does so by default on a non-loopback
# bind). Probing a real read endpoint would 403 there and look like a failed
# start. `Host:` is the probe host, which the viewer's Host allowlist accepts
# because it is either a loopback literal or the address we asked it to bind.
for _ in $(seq 1 30); do
  if curl -fsS "http://${PROBE_HOST}:${PORT}${BASE_PATH}/api/health.json" >/dev/null 2>&1; then
    echo "viewer_session=$SESSION_NAME"
    echo "viewer_url=http://${PROBE_HOST}:${PORT}${BASE_PATH}/"
    echo "viewer_bind=$BIND control_plane=$CONTROL read_token=$READ_TOKEN"
    if [[ "$CONTROL" != "0" ]]; then
      # The bootstrap URL is how the token reaches the browser. Read it out
      # of the token file the viewer just wrote rather than reimplementing
      # the generation here.
      token_file="$PROJECTS_ROOT/.trellis-viewer/control-token"
      if [[ -r "$token_file" ]]; then
        echo "viewer_control_url=http://${PROBE_HOST}:${PORT}${BASE_PATH}/$(cat "$token_file")/"
        echo "  (browse under that path for pause/resume and feedback; the plain URL stays read-only)"
      fi
    fi
    echo "viewer_tmux_socket=$TRELLIS_TMUX_SOCKET (attach: tmux -L $TRELLIS_TMUX_SOCKET attach -t $SESSION_NAME)"
    exit 0
  fi
  sleep 1
done

echo "viewer failed to start on port $PORT" >&2
exit 1
