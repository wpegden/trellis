#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PYTHONPATH="$ROOT_DIR${PYTHONPATH:+:$PYTHONPATH}"

# Worker-burst lean parallelism: bwrap strips the env, so sandbox.py reads
# this and forwards it to the burst as TRELLIS_LEAN_PARALLELISM. Decoupled
# from the supervisor's own TRELLIS_LEAN_PARALLELISM (which is baked at
# startup and can't be bumped without a restart). Picking 6 was empirically
# safe given <your-host>.example.com's 62 GB RAM headroom (per-lean-process
# peak ~0.8 GB; worst-case 12-way concurrent ≈ 9.6 GB).
: "${TRELLIS_BURST_LEAN_PARALLELISM:=6}"
export TRELLIS_BURST_LEAN_PARALLELISM

exec python3 -m trellis.runtime.bridge_cli "$@"
