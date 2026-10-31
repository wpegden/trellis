#!/usr/bin/env bash
# Run one already-initialized trust-v1 campaign through its sole human gate,
# let the runtime assemble and finalize the embedded-approval archive at
# PackageReady (Q7), and verify the finalized archive's embedded approval
# record.
#
# PV-framework rebuild (plan doc 32 Stage 4): the journal/keyset flow is
# retired.  The campaign binding is the launch-acknowledgment artifact
# rendered at bootstrap (LAUNCH_ACKNOWLEDGMENT.bin, hash-bound into state at
# seed pinning); the finalization witness is the runtime-root archive's
# embedded approval record, not a detached sidecar.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  cat <<'EOF'
Usage:
  scripts/run_trust_v1_campaign.sh \
    <runtime-root> <campaign-repo> <trust-root> <trust-cli-bin> <runtime-cli-bin>

The runtime must already have been initialized exactly once from the trust-v1
config produced by `trellis_trust_cli bootstrap-campaign`. The script waits at
the ordinary advance HumanGate until a fresh `human_gate_response.json` is
placed in the runtime root. It performs no human decision.
EOF
}

if [[ $# -ne 5 ]]; then
  usage >&2
  exit 2
fi

resolve_existing() {
  python3 - "$1" <<'PY'
from pathlib import Path
import sys
print(Path(sys.argv[1]).resolve(strict=True))
PY
}

RUNTIME_ROOT="$(resolve_existing "$1")"
CAMPAIGN_REPO="$(resolve_existing "$2")"
TRUST_ROOT="$(resolve_existing "$3")"
TRUST_CLI="$(resolve_existing "$4")"
RUNTIME_CLI="$(resolve_existing "$5")"

[[ -d "$RUNTIME_ROOT" && -f "$RUNTIME_ROOT/protocol_state.json" ]] || {
  echo "trust-v1 runner requires an initialized runtime root" >&2; exit 1; }
[[ -d "$CAMPAIGN_REPO/Tablet" && -d "$CAMPAIGN_REPO/crate" ]] || {
  echo "trust-v1 runner requires the pinned campaign repository" >&2; exit 1; }
[[ -x "$TRUST_CLI" && -x "$RUNTIME_CLI" ]] || {
  echo "trust-v1 runner requires executable pinned CLI binaries" >&2; exit 1; }

SEED_MANIFEST="$TRUST_ROOT/SEED_MANIFEST.json"
SEED_BUNDLE="$TRUST_ROOT/SEED_DEFINITION_BUNDLE.json"
EVIDENCE_ROOT="$TRUST_ROOT/evidence-root"
EVIDENCE_MANIFEST="$TRUST_ROOT/EVIDENCE_TOOL_MANIFEST.json"
LAUNCH_ACK="$TRUST_ROOT/LAUNCH_ACKNOWLEDGMENT.bin"
for required in \
  "$SEED_MANIFEST" "$SEED_BUNDLE" "$EVIDENCE_MANIFEST" "$LAUNCH_ACK"; do
  [[ -f "$required" ]] || { echo "missing trust-v1 input: $required" >&2; exit 1; }
done
[[ -d "$EVIDENCE_ROOT" ]] || { echo "missing evidence root: $EVIDENCE_ROOT" >&2; exit 1; }

# `verify-runtime-distribution` is record-only: it still refuses a corrupted
# frozen distribution, but live source and binary drift are reported instead of
# refused. This append-only trail is what carries toolchain identity forward —
# one JSON line per call, each pairing the emitted observation with the UTC
# time and the pass that asked for it.
DISTRIBUTION_PROVENANCE="$RUNTIME_ROOT/trust-v1-control/distribution-provenance.jsonl"

verify_runtime_distribution() {
  local pass_name="$1"
  local observation
  observation="$("$TRUST_CLI" verify-runtime-distribution \
    "$SEED_MANIFEST" "$EVIDENCE_MANIFEST" "$EVIDENCE_ROOT" \
    "$ROOT_DIR" "$RUNTIME_CLI")"
  printf '%s\n' "$observation"
  mkdir -p "$(dirname "$DISTRIBUTION_PROVENANCE")"
  TRELLIS_DISTRIBUTION_OBSERVATION="$observation" \
    python3 - "$DISTRIBUTION_PROVENANCE" "$pass_name" <<'PY'
import datetime
import json
import os
import sys

path, pass_name = sys.argv[1], sys.argv[2]
record = {
    "recorded_at_utc": datetime.datetime.now(datetime.timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    ),
    "pass": pass_name,
    "observation": json.loads(os.environ["TRELLIS_DISTRIBUTION_OBSERVATION"]),
}
with open(path, "a", encoding="utf-8") as trail:
    trail.write(json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")
PY
}

# The checker and supervisor still record the distribution they are about to
# consume before either starts.
verify_runtime_distribution launch

# The launch acknowledgment is the campaign binding (Stage 4): the bootstrap
# step rendered it and seed pinning wrote its tagged hash into state. Print
# the state-bound hash so the operator can compare it with the bootstrap
# output.
launch_ack_state_hash() {
  python3 - "$RUNTIME_ROOT/protocol_state.json" <<'PY'
import json
import sys

state = json.load(open(sys.argv[1], encoding="utf-8"))
trust = state.get("trust_base") or {}
print(trust.get("launch_acknowledgment_sha256") or "")
PY
}
LAUNCH_ACK_SHA256="$(launch_ack_state_hash)"
if [[ -z "$LAUNCH_ACK_SHA256" ]]; then
  echo "runtime state carries no launch-acknowledgment binding; re-run init from the bootstrap config" >&2
  exit 1
fi
echo "launch_acknowledgment=$LAUNCH_ACK" >&2
echo "launch_acknowledgment_sha256=$LAUNCH_ACK_SHA256" >&2

CONTROL_DIR="$RUNTIME_ROOT/trust-v1-control"
mkdir -p "$CONTROL_DIR"
CHECKER_SOCKET="$RUNTIME_ROOT/sockets/checker.sock"
if [[ -e "$CHECKER_SOCKET" ]]; then
  echo "refusing to replace an existing checker socket: $CHECKER_SOCKET" >&2
  exit 1
fi
CHECKER_LOG="$CONTROL_DIR/checker-server.log"
PYTHONPATH="$ROOT_DIR${PYTHONPATH:+:$PYTHONPATH}" \
  "$ROOT_DIR/scripts/trellis_checker_server.sh" "$RUNTIME_ROOT" \
  >>"$CHECKER_LOG" 2>&1 &
CHECKER_PID=$!
stop_checker() {
  if kill -0 "$CHECKER_PID" 2>/dev/null; then
    kill "$CHECKER_PID" 2>/dev/null || true
    wait "$CHECKER_PID" 2>/dev/null || true
  fi
}
trap stop_checker EXIT INT TERM

CHECKER_READY=0
for _ in $(seq 1 30); do
  if [[ -S "$CHECKER_SOCKET" ]]; then
    CHECKER_READY=1
    break
  fi
  if ! kill -0 "$CHECKER_PID" 2>/dev/null; then
    break
  fi
  sleep 1
done
if [[ "$CHECKER_READY" -ne 1 ]]; then
  echo "trust-v1 checker did not become ready; inspect $CHECKER_LOG" >&2
  exit 1
fi

runtime_status() {
  python3 - "$RUNTIME_ROOT/protocol_state.json" <<'PY'
import json
import sys

state = json.load(open(sys.argv[1], encoding="utf-8"))
trust = state.get("trust_base") or {}
phase = str(state.get("phase", "")).lower()
stage = str(state.get("stage", "")).lower()
gate_state = str(trust.get("routine_gate_state", "")).lower()
if gate_state == "feedbackterminated" or gate_state == "feedback_terminated":
    print("feedback_terminated")
elif (
    phase == "complete"
    and stage == "complete"
    and not trust.get("package_ready", False)
    and trust.get("package_finalization")
):
    # Q7: the finalization witness is the state-resident
    # `package_finalization` record re-verified inside pure apply_event.
    print("complete")
elif (
    phase == "cleanup"
    and stage == "start"
    and trust.get("package_ready") is True
):
    print("package_ready")
else:
    print("other")
PY
}

run_supervisor_pass() {
  local pass_name="$1"
  local output
  verify_runtime_distribution "$pass_name"
  output="$(mktemp "$CONTROL_DIR/${pass_name}.XXXXXX.json")"
  echo "trust-v1: starting supervisor pass $pass_name" >&2
  TRELLIS_CHECKER_SOCKET="$CHECKER_SOCKET" \
  TRELLIS_SOUNDNESS_FINGERPRINT_MODE=v2_strict \
  TRELLIS_TRELLIS_KERNEL_CMD="$RUNTIME_CLI" \
    "$ROOT_DIR/scripts/trellis.sh" run "$RUNTIME_ROOT" | tee "$output"
}

status="$(runtime_status)"
if [[ "$status" == "other" ]]; then
  run_supervisor_pass campaign
  status="$(runtime_status)"
fi
if [[ "$status" != "package_ready" && "$status" != "complete" ]]; then
  if [[ "$status" == "feedback_terminated" ]]; then
    echo "the advance-gate response rejected the seed; this run is terminal and no package is finalized" >&2
    exit 3
  fi
  echo "supervisor stopped outside PackageReady/Complete; inspect runtime markers and control logs" >&2
  exit 1
fi

# Q7 (plan doc 32 Stage 3): at PackageReady the RUNTIME assembles the
# finalization archive at the one deterministic RuntimePaths location inside
# the runtime root, embeds the approval record, and finalizes with the
# `FinalizeAuthorizedPackage` event on the next pass. No external
# build/authorize/sidecar step exists.
ARCHIVE="$RUNTIME_ROOT/trust_package.archive"
if [[ "$status" == "package_ready" ]]; then
  run_supervisor_pass finalize
  status="$(runtime_status)"
fi

if [[ "$status" != "complete" ]]; then
  echo "package-ready runtime did not finalize to Complete" >&2
  exit 1
fi
[[ -f "$ARCHIVE" ]] || {
  echo "finalized runtime root lacks the deterministic archive: $ARCHIVE" >&2; exit 1; }
# The final verification checks the archive's EMBEDDED approval record (Q7)
# instead of the retired verify-package + sidecar flow.
"$TRUST_CLI" verify-finalization-archive "$ARCHIVE"
echo "trust-v1 campaign complete" >&2
echo "archive=$ARCHIVE"
echo "launch_acknowledgment_sha256=$LAUNCH_ACK_SHA256"
