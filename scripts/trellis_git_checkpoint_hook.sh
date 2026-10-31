#!/usr/bin/env bash
set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PYTHONPATH="$ROOT_DIR${PYTHONPATH:+:$PYTHONPATH}"

# Parse stdin once and share the payload with optional stop/config actions.
# Checkpoint payloads can be hundreds of MB; do not spool or reparse them.
exec python3 -m trellis.runtime.checkpoint_hook_actions
