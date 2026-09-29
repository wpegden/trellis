#!/usr/bin/env bash
# Shared launch plumbing for create/resume jobs. Source this file.

trellis_load_lean_parallelism() {
  local value
  value="$(python3 - "$1" <<'PY'
import json, sys
value = json.load(open(sys.argv[1])).get("lean_parallelism")
if value is not None:
    if (isinstance(value, bool) or not isinstance(value, (int, str))
            or not str(value).isascii() or not str(value).isdecimal()
            or str(value).startswith("0") or not 1 <= int(value) <= 9007199254740991):
        raise SystemExit("lean_parallelism must be a positive integer")
    print(value)
PY
  )" || return 1
  if [[ -n "$value" ]]; then
    export TRELLIS_LEAN_PARALLELISM="$value"
    export TRELLIS_BURST_LEAN_PARALLELISM="$value"
    export LEAN_NUM_THREADS="$value"
  fi
}

trellis_parallelism_tmux() {
  # A tmux server retains its original environment. Exporting in the caller
  # alone does not configure a checker/supervisor/sidecar launched in a new
  # session, so pass the per-run settings explicitly without changing the
  # server environment shared by other runs.
  if [[ "${1:-}" == new-session && -n "${TRELLIS_LEAN_PARALLELISM:-}" ]]; then
    shift
    tmux -L "${TRELLIS_TMUX_SOCKET:-trellis}" new-session \
      -e "TRELLIS_LEAN_PARALLELISM=$TRELLIS_LEAN_PARALLELISM" \
      -e "TRELLIS_BURST_LEAN_PARALLELISM=${TRELLIS_BURST_LEAN_PARALLELISM:-$TRELLIS_LEAN_PARALLELISM}" \
      -e "LEAN_NUM_THREADS=${LEAN_NUM_THREADS:-$TRELLIS_LEAN_PARALLELISM}" "$@"
  else
    tmux -L "${TRELLIS_TMUX_SOCKET:-trellis}" "$@"
  fi
}
