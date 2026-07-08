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

tmux_cmd new-session -d -s "$SESSION_NAME" \
  "cd '$VIEWER_DIR' && PORT='$PORT' BASE_PATH='$BASE_PATH' PROMPTS_BASE='$PROMPTS_BASE' PROJECTS_ROOT='$PROJECTS_ROOT' STATIC_OUT='$STATIC_OUT' TRELLIS_TMUX_SOCKET='$TRELLIS_TMUX_SOCKET' node server.js"

for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:${PORT}${PROMPTS_BASE}/api/catalog.json" >/dev/null 2>&1; then
    echo "viewer_session=$SESSION_NAME"
    echo "viewer_url=http://127.0.0.1:${PORT}${BASE_PATH}/"
    echo "viewer_tmux_socket=$TRELLIS_TMUX_SOCKET (attach: tmux -L $TRELLIS_TMUX_SOCKET attach -t $SESSION_NAME)"
    exit 0
  fi
  sleep 1
done

echo "viewer failed to start on port $PORT" >&2
exit 1
