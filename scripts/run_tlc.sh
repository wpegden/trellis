#!/usr/bin/env bash
set -euo pipefail

# Trellis supervisor TLA+ runner. SIM ONLY.
#
# BFS exhaustive harnesses (small, medium) were removed deliberately — see
# spec/README.md "Sim-only policy". Do NOT re-add BFS configs or harnesses. If you need to exercise the spec
# more, tune the TLC_SIM_* env vars below — and keep TLC concurrency low
# (≤2 concurrent runs, monitor RAM).

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SPEC_DIR="$ROOT_DIR/spec"

TLA_VERSION="${TLA_VERSION:-1.7.2}"
TLA_CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/trellis/tla"
TLA_JAR_DEFAULT="$TLA_CACHE_DIR/tla2tools-v${TLA_VERSION}.jar"
TLA_JAR="${TLA2TOOLS_JAR:-$TLA_JAR_DEFAULT}"

SIM_NUM="${TLC_SIM_NUM:-100000}"
SIM_DEPTH="${TLC_SIM_DEPTH:-40}"
SIM_SEED="${TLC_SIM_SEED:-1}"
SIM_ARIL="${TLC_SIM_ARIL:-1}"
TLC_WORKERS="${TLC_WORKERS:-2}"
TLC_ACTIVE_PROCESSORS="${TLC_ACTIVE_PROCESSORS:-$TLC_WORKERS}"
TLC_STATE_ROOT="${TLC_STATE_ROOT:-$SPEC_DIR/states}"
TLC_TMPDIR="${TLC_TMPDIR:-$ROOT_DIR/.trellis/tlc_tmp}"

usage() {
  cat <<EOF
Usage:
  scripts/run_tlc.sh sim

The only supported mode is sim. It first runs the small SupervisorCore
bounded BFS (which carries the GapResearch gap-loop invariants — that spec
is small enough to search exhaustively), then the SupervisorProtocol random
simulation. The large Protocol BFS exhaustive harnesses were removed.

Environment:
  TLA2TOOLS_JAR  Override the path to tla2tools.jar
  TLA_VERSION    Release tag used when downloading tla2tools.jar (default: ${TLA_VERSION})
  TLC_SIM_NUM    Simulation trace count (default: ${SIM_NUM}). For iterative
                 work use TLC_SIM_NUM=2000 to keep wall time short.
  TLC_SIM_DEPTH  Simulation depth (default: ${SIM_DEPTH})
  TLC_SIM_SEED   Simulation seed (default: ${SIM_SEED})
  TLC_SIM_ARIL   Simulation aril (default: ${SIM_ARIL})
  TLC_WORKERS    TLC worker count (default: ${TLC_WORKERS}). Lower to 1 if
                 you're already running another TLC.
  TLC_ACTIVE_PROCESSORS  JVM processor cap (default: ${TLC_ACTIVE_PROCESSORS})
  TLC_STATE_ROOT TLC state directory root (default: ${TLC_STATE_ROOT})
  TLC_TMPDIR     JVM temp directory (default: ${TLC_TMPDIR})

RAM warning: a single sim run can exhaust JVM heap on this spec under default
params. Watch \`free -h\` while it runs. Never start a sim run while two are
already in flight.
EOF
}

ensure_tlc_jar() {
  if [[ -f "$TLA_JAR" ]]; then
    return
  fi

  mkdir -p "$(dirname "$TLA_JAR")"
  curl -L --fail \
    "https://github.com/tlaplus/tlaplus/releases/download/v${TLA_VERSION}/tla2tools.jar" \
    -o "$TLA_JAR"
}

run_sim() {
  local state_dir="$TLC_STATE_ROOT/sim"
  mkdir -p "$state_dir"
  mkdir -p "$TLC_TMPDIR"
  java -XX:+UseParallelGC "-XX:ActiveProcessorCount=${TLC_ACTIVE_PROCESSORS}" "-Djava.io.tmpdir=${TLC_TMPDIR}" -jar "$TLA_JAR" \
    -metadir "$state_dir" \
    -workers "${TLC_WORKERS}" \
    -simulate "num=${SIM_NUM}" \
    -depth "${SIM_DEPTH}" \
    -seed "${SIM_SEED}" \
    -aril "${SIM_ARIL}" \
    "$SPEC_DIR/SupervisorProtocolSim.tla" \
    -config "$SPEC_DIR/SupervisorProtocol.sim.cfg"
}

# The GapResearch planner ↔ critic loop is modeled on SupervisorCore, NOT on
# SupervisorProtocol, so its invariants (GapLoopTerminates,
# GapStageRejectConsistency) cannot live in SupervisorProtocol.sim.cfg. The
# Core harness is small enough for a full bounded BFS (≈25s, ~474k distinct
# states, depth 23 — not RAM-heavy like the Protocol spec), so a routine
# `run_tlc.sh sim` runs it exhaustively to keep those gap invariants in
# continuous coverage. This IS the exhaustive SupervisorCoreSim check.
run_core_bfs() {
  local state_dir="$TLC_STATE_ROOT/core_bfs"
  mkdir -p "$state_dir"
  mkdir -p "$TLC_TMPDIR"
  java -XX:+UseParallelGC "-XX:ActiveProcessorCount=${TLC_ACTIVE_PROCESSORS}" "-Djava.io.tmpdir=${TLC_TMPDIR}" -jar "$TLA_JAR" \
    -metadir "$state_dir" \
    -workers "${TLC_WORKERS}" \
    "$SPEC_DIR/SupervisorCoreSim.tla" \
    -config "$SPEC_DIR/SupervisorCoreSim.cfg"
}

# Add-targets init-at-complete harnesses (spec/AddTargets*Harness.tla).
# The reachable bounded spaces never fire the add-targets operator action
# (complete-phase + an unconfigured spare label are both out of reach), so
# these tiny exhaustive checks (<5s each, <20 states) start DIRECTLY at a
# synthetic complete-state and fire it — the harness class that catches
# successor-not-completely-specified bugs in operator-action disjuncts.
run_add_targets_harnesses() {
  local state_dir="$TLC_STATE_ROOT/add_targets_harness"
  mkdir -p "$state_dir/core" "$state_dir/protocol"
  mkdir -p "$TLC_TMPDIR"
  java -XX:+UseParallelGC "-XX:ActiveProcessorCount=${TLC_ACTIVE_PROCESSORS}" "-Djava.io.tmpdir=${TLC_TMPDIR}" -jar "$TLA_JAR" \
    -metadir "$state_dir/core" \
    -workers "${TLC_WORKERS}" \
    "$SPEC_DIR/AddTargetsCoreHarness.tla" \
    -config "$SPEC_DIR/AddTargetsCoreHarness.cfg"
  java -XX:+UseParallelGC "-XX:ActiveProcessorCount=${TLC_ACTIVE_PROCESSORS}" "-Djava.io.tmpdir=${TLC_TMPDIR}" -jar "$TLA_JAR" \
    -metadir "$state_dir/protocol" \
    -workers "${TLC_WORKERS}" \
    "$SPEC_DIR/AddTargetsProtocolHarness.tla" \
    -config "$SPEC_DIR/AddTargetsProtocolHarness.cfg"
}

# Parallel-closure sidecar init-at-enabled harness (spec/SidecarCoreHarness.tla):
# the stock SupervisorCoreSim bounds never enable ApplySidecarClosure (single
# node + active-node exclusion), so this tiny exhaustive check (<5s, 4 states)
# starts at a synthetic ProofFormalization boundary and FIRES it.
run_sidecar_harness() {
  local state_dir="$TLC_STATE_ROOT/sidecar_harness"
  mkdir -p "$state_dir"
  mkdir -p "$TLC_TMPDIR"
  java -XX:+UseParallelGC "-XX:ActiveProcessorCount=${TLC_ACTIVE_PROCESSORS}" "-Djava.io.tmpdir=${TLC_TMPDIR}" -jar "$TLA_JAR" \
    -metadir "$state_dir" \
    -workers "${TLC_WORKERS}" \
    "$SPEC_DIR/SidecarCoreHarness.tla" \
    -config "$SPEC_DIR/SidecarCoreHarness.cfg"
}

# Proof-formalization substantiveness-reset harness
# (spec/ProofResetProtocolHarness.tla): the stock simulation does not reach a
# proof-formalization Reviewer boundary carrying live substantiveness Fails, so
# the reset offer added there would never be exercised. This tiny exhaustive
# check (<5s, 4 states) starts AT that boundary and fires the transition —
# check the -coverage line for HApplyProof.
run_proof_reset_harness() {
  local state_dir="$TLC_STATE_ROOT/proof_reset_harness"
  mkdir -p "$state_dir"
  mkdir -p "$TLC_TMPDIR"
  java -XX:+UseParallelGC "-XX:ActiveProcessorCount=${TLC_ACTIVE_PROCESSORS}" "-Djava.io.tmpdir=${TLC_TMPDIR}" -jar "$TLA_JAR" \
    -metadir "$state_dir" \
    -workers "${TLC_WORKERS}" \
    -coverage 1 \
    "$SPEC_DIR/ProofResetProtocolHarness.tla" \
    -config "$SPEC_DIR/ProofResetProtocolHarness.cfg"
}

mode="${1:-sim}"
case "$mode" in
  sim)
    ;;
  small|medium|all)
    echo "error: '$mode' mode was removed. Sim is the only supported mode." >&2
    echo "       See spec/README.md and memory feedback_tla_sim_only.md." >&2
    exit 2
    ;;
  *)
    usage
    exit 2
    ;;
esac

ensure_tlc_jar
run_add_targets_harnesses
run_sidecar_harness
run_proof_reset_harness
run_core_bfs
run_sim
