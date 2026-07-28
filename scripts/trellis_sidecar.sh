#!/usr/bin/env bash
# Parallel-closure sidecar daemon launcher.
#
# Usage:
#   scripts/trellis_sidecar.sh <runtime_root> --repo <live_repo> [--config <trellis.config.json>]
#   scripts/trellis_sidecar.sh status <runtime_root> [--json]
#
# `status` answers "is it alive?" from the daemon's own pid LOCK, never
# from a process-name match: `pgrep -f trellis.sidecar` is an ERE (the
# `.` also matches `trellis_sidecar.sh`) and it matches every
# `trellis.sidecar.attempt` CHILD, so a dead manager whose children
# outlived it reads UP. Exit codes: 0 running+fresh, 1 not running,
# 2 degraded, 3 no sidecar dir, 4 undeterminable, 64 usage.
# See SIDECAR_OPERATIONS.md.
#
# Run it inside a tmux session on the shared trellis socket, named by
# the run slug (the checker/prewarm naming convention):
#   tmux -L trellis new-session -d -s "trellis-sidecar-<slug>" \
#       "scripts/trellis_sidecar.sh <runtime_root> --repo <live_repo>"
#
# The daemon is INERT (exits 0 immediately) unless trellis.config.json
# carries an enabled `sidecar` block. NOTE: the config edit must be
# COMMITTED to the run repo — a checkpoint `git reset --hard` restores
# tracked config, so an uncommitted enablement silently reverts.
#
# Two ways out:
#   touch <runtime_root>/sidecar/stop    hard stop — cancel + roll back
#                                        every in-flight attempt, then exit
#   touch <runtime_root>/sidecar/drain   drain — stop assigning and exit NOW,
#                                        leaving the attempt processes running;
#                                        the next daemon over this runtime root
#                                        adopts them off sidecar/slots.json
# `stop` wins if both are present. Both are unlinked as the daemon exits.
#
# Drain is the restart/redeploy path: it costs no in-flight grunt work. The
# adopted children keep running the code they were STARTED with, so relaunch
# from a NEW worktree instead of editing this tree underneath them, and never
# relaunch a build that predates adoption while attempts are still in flight.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PYTHONPATH="${ROOT_DIR}${PYTHONPATH:+:$PYTHONPATH}"

if [[ "${1:-}" == "status" ]]; then
  shift
  exec python3 -m trellis.sidecar.health "$@"
fi

exec python3 -m trellis.sidecar "$@"
