#!/usr/bin/env bash
# Launcher for the unified-checker UNIX-socket server.
#
# This server IS the live, always-on acceptance checker: it is the only
# supported way to run authoritative acceptance lake checks. Both
# scripts/init_new_run.sh and scripts/restart_configured_run.sh launch it
# (in a sibling `trellis-checker-<slug>` tmux session) and export
# TRELLIS_CHECKER_SOCKET into the supervisor's environment, so worker
# bursts and supervisor-side observations route acceptance through the
# socket. This launcher can also be invoked directly to spin the server up
# against an arbitrary runtime root for integration tests and
# forensic-replay diagnostics.
#
# Usage: scripts/trellis_checker_server.sh <runtime_root> [--peer-uid N] [--parallelism N]
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PYTHONPATH="$ROOT_DIR${PYTHONPATH:+:$PYTHONPATH}"

exec python3 -m trellis.checker.server "$@"
