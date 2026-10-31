#!/usr/bin/env bash
# State-machine test for the prose PV viewer job. Expensive extraction,
# bootstrap, and launch commands are shape-faithful stubs; archive handling,
# source materialization, tmux phase boundaries, freshness, and error relay are
# real.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_SCRATCH="${TRELLIS_PHASE0_TEST_SCRATCH:-$ROOT/scratch/upload-model}"
mkdir -p "$TEST_SCRATCH"
TEST_ROOT="$(mktemp -d "$TEST_SCRATCH/pv-create-test.XXXXXX")"
PROJECTS="$TEST_ROOT/projects"
STUBS="$TEST_ROOT/stubs"
SOCKET="pv-create-test-$$"
mkdir -p "$PROJECTS" "$STUBS"

cleanup() {
  PATH="$STUBS:$PATH" tmux -L "$SOCKET" kill-server >/dev/null 2>&1 || true
  rm -rf -- "$TEST_ROOT"
}
trap cleanup EXIT

cat > "$STUBS/tmux" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ "${1:-}" == "-L" ]] && shift 2
store="${TRELLIS_FAKE_TMUX_ROOT:?}"
case "${1:-}" in
  new-session)
    shift
    while [[ "${1:-}" == "-e" ]]; do export "$2"; shift 2; done
    [[ "${1:-}" == "-d" ]] && shift
    [[ "${1:-}" == "-s" ]] || exit 2
    name="$2"; shift 2
    if [[ -f "$store/$name.pid" ]] && kill -0 "$(cat "$store/$name.pid")" 2>/dev/null; then
      exit 1
    fi
    command="$*"
    nohup bash -c "$command" >/dev/null 2>&1 &
    printf '%s\n' "$!" > "$store/$name.pid"
    ;;
  list-sessions)
    for file in "$store"/*.pid; do
      [[ -e "$file" ]] || continue
      pid="$(cat "$file")"
      if kill -0 "$pid" 2>/dev/null; then basename "$file" .pid; else rm -f "$file"; fi
    done
    ;;
  kill-session)
    shift
    [[ "${1:-}" == "-t" ]] || exit 2
    file="$store/$2.pid"
    if [[ -f "$file" ]]; then kill "$(cat "$file")" 2>/dev/null || true; rm -f "$file"; fi
    ;;
  kill-server)
    for file in "$store"/*.pid; do
      [[ -e "$file" ]] || continue
      kill "$(cat "$file")" 2>/dev/null || true
      rm -f "$file"
    done
    ;;
  *) exit 2 ;;
esac
SH

cat > "$STUBS/extract" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
out=""
while [[ $# -gt 0 ]]; do
  case "$1" in --out) out="$2"; shift 2 ;; *) shift ;; esac
done
mkdir -p "$out/Tablet"
printf '%s\n' '{"verification_targets":[{"name":"alpha"}]}' > "$out/pv_tablet.json"
printf '%s\n' '-- generated' > "$out/Tablet/Preamble.lean"
SH

cat > "$STUBS/setup" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ "${RUST_REFERENCE_REV:-}" =~ ^[0-9a-f]{40}$ ]]
[[ "${NOMICON_REV:-}" =~ ^[0-9a-f]{40}$ ]]
[[ "${RUST_STD_REV:-}" =~ ^[0-9a-f]{40}$ ]]
crate=""; unadapted_crate=""; out=""; goal=""; genesis=""; seal=""
filespec_checker=""; prescribed_region_checker=""
config_template=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --crate) crate="$2"; shift 2 ;;
    --unadapted-crate) unadapted_crate="$2"; shift 2 ;;
    --out-repo) out="$2"; shift 2 ;;
    --goal) goal="$2"; shift 2 ;;
    --phase0-genesis) genesis="$2"; shift 2 ;;
    --sealed-generation) seal="$2"; shift 2 ;;
    --filespec-checker) filespec_checker="$2"; shift 2 ;;
    --prescribed-region-checker) prescribed_region_checker="$2"; shift 2 ;;
    --config-template) config_template="$2"; shift 2 ;;
    --runtime-root|--project-slug|--aeneas-require-mode|--cargo-target-dir|--work-root|--charon-bin|--aeneas-bin|--opam-switch) shift 2 ;;
    --defer-runtime-init) shift ;;
    *) exit 2 ;;
  esac
done
[[ -d "$crate" && -f "$crate/PHASE0_ADAPTED" && -d "$unadapted_crate" && ! -e "$out" ]]
[[ -f "$genesis" && -f "$seal" ]]
[[ -f "$config_template" ]]
[[ -x "$filespec_checker" && "$(basename "$filespec_checker")" == "scan_tablet_filespec" ]]
[[ -x "$prescribed_region_checker" && "$(basename "$prescribed_region_checker")" == "print_prescribed_region" ]]
printf 'setup\n' >> "${TRELLIS_PV_CREATE_ORDER_LOG:?}"
mkdir -p "$out/Tablet" "$out/.trellis/phase0"
cp -R "$crate" "$out/crate"
cp -R "$unadapted_crate" "$out/unadapted-crate"
cp "$goal" "$out/GOAL.md"
cp "$genesis" "$out/.trellis/phase0/GENESIS.json"
cp "$seal" "$out/.trellis/phase0/SEALED_GENERATION.json"
printf '{}\n' > "$out/.trellis/phase0/GOAL_BINDING_REPORT.json"
printf '%s\n' '{"verification_targets":[{"name":"alpha","resolution":"decide"}]}' > "$out/pv_tablet.json"
python3 - "$config_template" "$out/trellis.config.json" <<'PY'
import json, sys
config = json.load(open(sys.argv[1], encoding="utf-8"))
config["pv_tablet"] = {"verification_targets": [{"name": "alpha", "resolution": "decide"}]}
with open(sys.argv[2], "w", encoding="utf-8") as stream:
    json.dump(config, stream)
PY
SH

cat > "$STUBS/trust_cli" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
replay-phase0-adaptation)
  printf 'replay\n' >> "${TRELLIS_PV_CREATE_ORDER_LOG:?}"
  raw="$2"; candidate="$5"; out="$6"
  [[ -f "$3" && -f "$4" && -f "$candidate/PHASE0_ADAPTED" ]]
  if [[ "${TRELLIS_PV_CREATE_WRONG_REPLAY:-}" == "1" ]]; then cp -R "$raw" "$out"; else cp -R "$candidate" "$out"; fi
  ;;
verify-phase0-production)
  printf 'production-verify\n' >> "${TRELLIS_PV_CREATE_ORDER_LOG:?}"
  [[ -d "$2/crate" && -d "$2/unadapted-crate" && -f "$3" && -f "$4" ]]
  printf '{}\n' > "$5"
  ;;
bootstrap-campaign)
  printf 'bootstrap\n' >> "${TRELLIS_PV_CREATE_ORDER_LOG:?}"
  if [[ "${TRELLIS_PV_CREATE_BOOTSTRAP_REFUSE:-}" == "1" ]]; then
    echo 'kernel refusal: Preamble contains an undisclosed declaration' >&2
    exit 19
  fi
  [[ -f "$8" && -f "$9" ]]
python3 - "$2/trellis.config.json" <<'PY'
import json, sys
config = json.load(open(sys.argv[1], encoding="utf-8"))
assert config.get("system_feedback_halt") is True, config
PY
trust_root="$5"
mkdir -p "$trust_root/evidence-root/bin"
printf '%s\n' '{"fixture":true}' > "$trust_root/trellis.config.trust-v1.json"
cat > "$trust_root/evidence-root/bin/trellis-runtime-kernel" <<'INNER'
#!/usr/bin/env bash
exit 0
INNER
chmod +x "$trust_root/evidence-root/bin/trellis-runtime-kernel"
  ;;
*) exit 2 ;;
esac
SH

cat > "$STUBS/phase0" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf 'phase0\n' >> "${TRELLIS_PV_CREATE_ORDER_LOG:?}"
[[ "${TRELLIS_PV_CREATE_PHASE0_FAIL:-}" != "1" ]] || exit 31
raw=""; attempt=""; authority=""; result=""
target_archive=""; support_archive=""; partition_manifest=""
filespec_checker=""; prescribed_region_checker=""
lane_config=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --unadapted-tree) raw="$2"; shift 2 ;;
    --target-archive) target_archive="$2"; shift 2 ;;
    --supporting-archive) support_archive="$2"; shift 2 ;;
    --partition-manifest) partition_manifest="$2"; shift 2 ;;
    --attempt-root) attempt="$2"; shift 2 ;;
    --authority-root) authority="$2"; shift 2 ;;
    --result-out) result="$2"; shift 2 ;;
    --filespec-checker) filespec_checker="$2"; shift 2 ;;
    --prescribed-region-checker) prescribed_region_checker="$2"; shift 2 ;;
    --lane-config) lane_config="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[[ -f "$target_archive" && -f "$partition_manifest" ]]
[[ -f "$lane_config" ]]
[[ -z "$support_archive" || -f "$support_archive" ]]
[[ -x "$filespec_checker" && "$(basename "$filespec_checker")" == "scan_tablet_filespec" ]]
[[ -x "$prescribed_region_checker" && "$(basename "$prescribed_region_checker")" == "print_prescribed_region" ]]
mkdir -p "$authority" "$attempt/candidate"
cp "$lane_config" "$authority/LANE_CONFIG.json"
cp -R "$raw/." "$attempt/candidate/"
touch "$attempt/candidate/PHASE0_ADAPTED"
printf '{}\n' > "$authority/GENESIS.json"
printf '{}\n' > "$authority/ADAPTATION_LEDGER.json"
printf '["goal:alpha"]\n' > "$authority/GOAL_TARGETS.json"
cp "$partition_manifest" "$authority/source-partition-manifest.json"
cp "$(dirname "$partition_manifest")/SOURCE_TREE_MANIFEST.json" "$authority/source-tree-manifest.json"
seal="$authority/SEALED_GENERATION.json"
if [[ "${TRELLIS_PV_CREATE_PHASE0_BOUND:-}" != "1" ]]; then printf '{}\n' > "$seal"; fi
python3 - "$result" "$authority" "$attempt/candidate" <<'PY'
import json, sys
from pathlib import Path
out, authority, candidate = map(Path, sys.argv[1:])
out.write_text(json.dumps({
    "genesis_path": str(authority / "GENESIS.json"),
    "sealed_generation_path": str(authority / "SEALED_GENERATION.json"),
    "ledger_path": str(authority / "ADAPTATION_LEDGER.json"),
    "goal_targets_path": str(authority / "GOAL_TARGETS.json"),
    "source_tree_manifest_path": str(authority / "source-tree-manifest.json"),
    "source_partition_manifest_path": str(authority / "source-partition-manifest.json"),
    "candidate_tree_path": str(candidate),
}), encoding="utf-8")
PY
SH

cat > "$STUBS/plan" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf 'plan\n' >> "${TRELLIS_PV_CREATE_ORDER_LOG:?}"
out=""
while [[ $# -gt 0 ]]; do case "$1" in --out) out="$2"; shift 2 ;; *) shift ;; esac; done
printf '%s' '{"adaptation_ledger":[],"schema":"trellis-campaign-trust-plan/v4","targets":[{"resolution":"decide","target_id":"goal:alpha"}]}' > "$out"
SH

cat > "$STUBS/init" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
config="$1"; runtime="$2"
[[ -f "$config" && ! -e "$runtime" ]]
mkdir -p "$runtime"
printf 'init\n' >> "${TRELLIS_PV_CREATE_ORDER_LOG:?}"
printf '%s\n' '{"phase":"theorem_stating","stage":"start"}' > "$runtime/protocol_state.json"
SH

cat > "$STUBS/gate" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ -f "$1/protocol_state.json" && -f "$2" && -d "$3/Tablet" ]]
printf 'gates\n' >> "${TRELLIS_PV_CREATE_ORDER_LOG:?}"
SH

cat > "$STUBS/runner" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ -f "$1/protocol_state.json" && -d "$2/Tablet" && -d "$3" && -x "$4" && -x "$5" ]]
[[ -z "${TRELLIS_SYSTEM_FEEDBACK_HALT:-}" ]]
printf 'runner\n' >> "${TRELLIS_PV_CREATE_ORDER_LOG:?}"
printf '%s|%s|%s\n' "${TRELLIS_LEAN_PARALLELISM:-}" "${TRELLIS_BURST_LEAN_PARALLELISM:-}" "${LEAN_NUM_THREADS:-}" > "$1/parallelism-observed"
python3 - "$2/trellis.config.json" <<'PY'
import json, sys
assert json.load(open(sys.argv[1], encoding="utf-8")).get("system_feedback_halt") is True
PY
sleep 60
SH
chmod +x "$STUBS"/*

cat > "$STUBS/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
target="${CARGO_TARGET_DIR:?}"
[[ " $* " == *" --bin trellis_trust_cli "* ]]
[[ " $* " == *" --example scan_tablet_filespec "* ]]
[[ " $* " == *" --example print_prescribed_region "* ]]
mkdir -p "$target/release/examples"
cp "${TRELLIS_FAKE_TMUX_ROOT:?}/stubs/trust_cli" "$target/release/trellis_trust_cli"
chmod +x "$target/release/trellis_trust_cli"
for checker in scan_tablet_filespec print_prescribed_region; do
  printf '%s\n' '#!/usr/bin/env bash' 'exit 0' > "$target/release/examples/$checker"
  chmod +x "$target/release/examples/$checker"
done
SH
chmod +x "$STUBS/cargo"

cat > "$STUBS/archive" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$2" >> "${TRELLIS_PV_CREATE_ARCHIVE_LOG:?}"
exec python3 "${TRELLIS_TEST_REPO_ROOT:?}/scripts/pv_archive.py" "$@"
SH

cat > "$STUBS/adapt" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf 'partition\n' >> "${TRELLIS_PV_CREATE_ORDER_LOG:?}"
exec python3 "${TRELLIS_TEST_REPO_ROOT:?}/scripts/adapt_pv_source.py" "$@"
SH
chmod +x "$STUBS/archive" "$STUBS/adapt"

TARGET="$TEST_ROOT/target"
SUPPORT="$TEST_ROOT/support"
mkdir -p "$TARGET/src" "$SUPPORT"
printf '%s\n' '[package]' 'name = "fixture"' 'version = "0.1.0"' > "$TARGET/Cargo.toml"
printf '%s\n' 'pub fn alpha() {}' > "$TARGET/src/lib.rs"
printf '%s\n' 'pub trait Support {}' > "$SUPPORT/traits.rs"
tar -czf "$TEST_ROOT/target.tar.gz" -C "$TARGET" .
tar -czf "$TEST_ROOT/support.tar.gz" -C "$SUPPORT" .
printf '%s\n' '# GOAL' '### `alpha`' 'Verify alpha.' > "$TEST_ROOT/GOAL.md"

# The phase-time Python validator independently rejects traversal and removes
# the destination it began to create; intake validation is not trusted alone.
python3 - "$TEST_ROOT/traversal.tar.gz" <<'PY'
import io, sys, tarfile
with tarfile.open(sys.argv[1], "w:gz") as archive:
    data = b"[package]\nname='escape'\nversion='0.1.0'\n"
    member = tarfile.TarInfo("../Cargo.toml")
    member.size = len(data)
    archive.addfile(member, io.BytesIO(data))
PY
if python3 "$ROOT/scripts/pv_archive.py" extract "$TEST_ROOT/traversal.tar.gz" \
    "$TEST_ROOT/must-not-extract" --limit 20971520 >/dev/null 2>&1; then
  echo "phase-time archive validator accepted traversal" >&2
  exit 1
fi
[[ ! -e "$TEST_ROOT/must-not-extract" ]]

export PATH="$STUBS:$PATH"
export TRELLIS_FAKE_TMUX_ROOT="$TEST_ROOT"
export TRELLIS_PROJECTS_ROOT="$PROJECTS"
export TRELLIS_TMUX_SOCKET="$SOCKET"
export TRELLIS_CREATE_HEARTBEAT_SECS=1
export TRELLIS_CREATE_VERIFY_SECS=6
export TRELLIS_TEST_REPO_ROOT="$ROOT"
export TRELLIS_PV_CREATE_ARCHIVE_LOG="$TEST_ROOT/archive.log"
export TRELLIS_PV_CREATE_ARCHIVE_CMD="$STUBS/archive"
export TRELLIS_PV_CREATE_ADAPT_CMD="$STUBS/adapt"
export TRELLIS_PV_CREATE_PHASE0_CMD="$STUBS/phase0"
export TRELLIS_PV_CREATE_PLAN_CMD="$STUBS/plan"
export TRELLIS_PV_CREATE_SETUP_CMD="$STUBS/setup"
export TRELLIS_PV_CREATE_INIT_CMD="$STUBS/init"
export TRELLIS_PV_CREATE_GATE_CMD="$STUBS/gate"
export TRELLIS_PV_CREATE_RUNNER="$STUBS/runner"
export TRELLIS_PV_CREATE_ORDER_LOG="$TEST_ROOT/order.log"
export TRELLIS_SYSTEM_FEEDBACK_HALT=0
: > "$TRELLIS_PV_CREATE_ORDER_LOG"
: > "$TRELLIS_PV_CREATE_ARCHIVE_LOG"

CREATE="$ROOT/scripts/pv_create_run.sh"
if grep -q 'TRELLIS_PV_CREATE_PLAN_BUILDER' "$CREATE"; then
  echo "create script still advertises the nonexistent plan-builder override" >&2
  exit 1
fi
good_ack="$(bash "$CREATE" start good --goal "$TEST_ROOT/GOAL.md" \
  --target "$TEST_ROOT/target.tar.gz" --support "$TEST_ROOT/support.tar.gz" \
  --role-provider phase0.adapting_worker=kimi --role-base-url phase0.adapting_worker=https://api.moonshot.cn/v1 \
  --role-model phase0.seam_repair_auditor=p0-seam --role-effort phase0.seam_repair_auditor=high \
  --role-model phase0.correspondence_agents=p0-corr --role-effort phase0.correspondence_agents=xhigh \
  --role-model worker=run-worker --role-effort worker=high \
  --role-model easy_worker=run-easy --role-effort easy_worker=xhigh \
  --role-model hard_worker=run-hard --role-effort hard_worker=high \
  --role-model reviewer=run-review --role-effort reviewer=xhigh \
  --role-model stuck_math_audit=run-audit --role-effort stuck_math_audit=high \
  --role-model verification.correspondence_agents=run-corr --role-effort verification.correspondence_agents=xhigh \
  --role-model verification.soundness_agents=run-sound --role-effort verification.soundness_agents=high \
  --role-model verification.substantiveness_agents=run-subst --role-effort verification.substantiveness_agents=xhigh \
  --lean-parallelism 4 --remote-url git@example.test:team/pv.git \
  --allow-same-model-lanes on)"
grep -q 'phase0.adapting_worker=' <<<"$good_ack"
grep -q 'phase0.allow_same_model_lanes=true' <<<"$good_ack"
for _ in $(seq 1 100); do
  state="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"])' "$PROJECTS/.trellis-viewer/create-jobs/good/status.json")"
  [[ "$state" == "done" ]] && break
  [[ "$state" == *_failed ]] && { cat "$PROJECTS/.trellis-viewer/create-jobs/good/create.log"; exit 1; }
  sleep 0.1
done
[[ "$state" == "done" ]]
[[ "$(cat "$TRELLIS_PV_CREATE_ORDER_LOG")" == $'partition\nphase0\nreplay\nsetup\nproduction-verify\nplan\nbootstrap\ninit\ngates\nrunner' ]]
GOOD_JOB="$PROJECTS/.trellis-viewer/create-jobs/good"
[[ "$(cat "$PROJECTS/good-runtime/parallelism-observed")" == '4|4|4' ]]
[[ "$(cat "$TRELLIS_PV_CREATE_ARCHIVE_LOG")" == $GOOD_JOB/target.tar.gz$'\n'$GOOD_JOB/support.tar.gz ]]
if find "$PROJECTS/.trellis-viewer/create-jobs/good" -maxdepth 1 -name 'attempt.*' | grep -q .; then
  echo "successful create retained an end-to-end attempt tree" >&2
  exit 1
fi
[[ -d "$PROJECTS/good/Tablet" && -f "$PROJECTS/good-runtime/protocol_state.json" ]]
[[ ! -e "$PROJECTS/good/.trellis-creating" ]]
grep -q 'recorded campaign property system_feedback_halt=true' \
  "$PROJECTS/.trellis-viewer/create-jobs/good/create.log"
python3 - \
  "$PROJECTS/.trellis-viewer/create-jobs/good/job.json" \
  "$PROJECTS/good/trellis.config.json" \
  "$PROJECTS/.trellis-viewer/create-jobs/good/authority/phase0/LANE_CONFIG.json" <<'PY'
import json, sys
job = json.load(open(sys.argv[1], encoding="utf-8"))
config = json.load(open(sys.argv[2], encoding="utf-8"))
phase0_config = json.load(open(sys.argv[3], encoding="utf-8"))
assert job.get("system_feedback_halt") is True, job
assert job.get("target") == "target.tar.gz", job
assert job.get("supporting") == "support.tar.gz", job
assert "crate" not in job, job
assert config.get("system_feedback_halt") is True, config
expected = {
    "phase0.adapting_worker": ("kimi-k3", "max"),
    "phase0.seam_repair_auditor": ("p0-seam", "high"),
    "phase0.correspondence_agents": ("p0-corr", "xhigh"),
    "worker": ("run-worker", "high"),
    "easy_worker": ("run-easy", "xhigh"),
    "hard_worker": ("run-hard", "high"),
    "reviewer": ("run-review", "xhigh"),
    "stuck_math_audit": ("run-audit", "high"),
    "verification.correspondence_agents": ("run-corr", "xhigh"),
    "verification.soundness_agents": ("run-sound", "high"),
    "verification.substantiveness_agents": ("run-subst", "xhigh"),
}
assert set(job["role_overrides"]) == set(expected), job["role_overrides"]
for role, (model, effort) in expected.items():
    if role == "phase0.adapting_worker":
        assert job["role_overrides"][role] == {"provider": "kimi", "base_url": "https://api.moonshot.cn/v1"}
        assert config["phase0"]["adapting_worker"]["provider"] == "kimi"
        assert config["api_providers"]["kimi"]["base_url"] == "https://api.moonshot.cn/v1"
    else:
        assert job["role_overrides"][role] == {"provider": "codex", "model": model, "effort": effort}
    value = config
    for part in role.split("."):
        if isinstance(value, list):
            value = [entry[part] for entry in value]
        else:
            value = value[part]
    for entry in value if isinstance(value, list) else [value]:
        assert entry["model"] == model, (role, entry)
        assert entry["effort"] == effort, (role, entry)
assert job["allow_same_model_lanes"] is True
assert config["phase0"]["allow_same_model_lanes"] is True
assert phase0_config["phase0"] == config["phase0"]
assert config["git"]["remote_url"] == "git@example.test:team/pv.git"
print("launched_config.system_feedback_halt=true")
PY
python3 - "$PROJECTS/.trellis-viewer/create-jobs/good/authority/phase0/source-partition-manifest.json" <<'PY'
import json, sys
manifest = json.load(open(sys.argv[1], encoding="utf-8"))
origins = {row["relative_path"]: row["origin"] for row in manifest["files"]}
assert origins == {
    "Cargo.toml": "target",
    "src/lib.rs": "target",
    "traits.rs": "supporting",
}, origins
assert manifest["target_source_tree_sha256"]
assert manifest["supporting_source_tree_sha256"]
assert manifest["partition_manifest_sha256"]
PY

# The target partition owns its paths. A supporting archive may not replace or
# duplicate any of them, even with byte-identical content.
OVERLAP="$TEST_ROOT/overlap"
mkdir -p "$OVERLAP/src"
cp "$TARGET/src/lib.rs" "$OVERLAP/src/lib.rs"
printf '%s\n' 'pub fn dummy() {}' > "$OVERLAP/dummy.rs"
tar -czf "$TEST_ROOT/overlap.tar.gz" -C "$OVERLAP" .
: > "$TRELLIS_PV_CREATE_ORDER_LOG"
bash "$CREATE" start overlap --goal "$TEST_ROOT/GOAL.md" \
  --target "$TEST_ROOT/target.tar.gz" --support "$TEST_ROOT/overlap.tar.gz"
for _ in $(seq 1 100); do
  state="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"])' "$PROJECTS/.trellis-viewer/create-jobs/overlap/status.json")"
  [[ "$state" == "resolve_failed" ]] && break
  sleep 0.1
done
[[ "$state" == "resolve_failed" ]]
[[ "$(cat "$TRELLIS_PV_CREATE_ORDER_LOG")" == "partition" ]]
grep -q 'target and supporting uploads overlap' \
  "$PROJECTS/.trellis-viewer/create-jobs/overlap/create.log"
if find "$PROJECTS/.trellis-viewer/create-jobs/overlap" -maxdepth 1 -name 'attempt.*' | grep -q .; then
  echo "overlap refusal retained an end-to-end attempt tree" >&2
  exit 1
fi

# No setup, production extraction binding, plan, bootstrap, init, gates, or
# runner is reachable before a sealed generation. Both a hard Phase-0 failure
# and a non-sealed (bound/incomplete) result clean the complete attempt tree.
for scenario in phasefail bound; do
  : > "$TRELLIS_PV_CREATE_ORDER_LOG"
  if [[ "$scenario" == phasefail ]]; then
    export TRELLIS_PV_CREATE_PHASE0_FAIL=1
  else
    export TRELLIS_PV_CREATE_PHASE0_BOUND=1
  fi
  bash "$CREATE" start "$scenario" --goal "$TEST_ROOT/GOAL.md" --target "$TEST_ROOT/target.tar.gz"
  unset TRELLIS_PV_CREATE_PHASE0_FAIL TRELLIS_PV_CREATE_PHASE0_BOUND
  for _ in $(seq 1 100); do
    state="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"])' "$PROJECTS/.trellis-viewer/create-jobs/$scenario/status.json")"
    [[ "$state" == "resolve_failed" ]] && break
    sleep 0.1
  done
  [[ "$state" == "resolve_failed" ]]
  [[ "$(cat "$TRELLIS_PV_CREATE_ORDER_LOG")" == $'partition\nphase0' ]]
  if find "$PROJECTS/.trellis-viewer/create-jobs/$scenario" -maxdepth 1 -name 'attempt.*' | grep -q .; then
    echo "$scenario retained an end-to-end attempt tree" >&2
    exit 1
  fi
done

# The setup boundary receives only the fresh replay destination. A replay
# command that materializes the wrong root is rejected before production
# verification and all later phases.
: > "$TRELLIS_PV_CREATE_ORDER_LOG"
export TRELLIS_PV_CREATE_WRONG_REPLAY=1
bash "$CREATE" start wrongreplay --goal "$TEST_ROOT/GOAL.md" --target "$TEST_ROOT/target.tar.gz"
unset TRELLIS_PV_CREATE_WRONG_REPLAY
for _ in $(seq 1 100); do
  state="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"])' "$PROJECTS/.trellis-viewer/create-jobs/wrongreplay/status.json")"
  [[ "$state" == "build_failed" ]] && break
  sleep 0.1
done
[[ "$state" == "build_failed" ]]
[[ "$(cat "$TRELLIS_PV_CREATE_ORDER_LOG")" == $'partition\nphase0\nreplay' ]]
if find "$PROJECTS/.trellis-viewer/create-jobs/wrongreplay" -maxdepth 1 -name 'attempt.*' | grep -q .; then
  echo "wrong replay retained an end-to-end attempt tree" >&2
  exit 1
fi
python3 - "$PROJECTS/.trellis-viewer/create-jobs/good/authority/plan/TRUST_PLAN.json" <<'PY'
import json, sys
plan = json.load(open(sys.argv[1], encoding="utf-8"))
assert plan == {
    "schema": "trellis-campaign-trust-plan/v4",
    "adaptation_ledger": [],
    "targets": [{"target_id": "goal:alpha", "resolution": "decide"}],
}, plan
PY

# Delete mirrors the math flow: after exact path confirmation it stops a live
# runner before removing any path. Reusing the slug then reaches a fresh launch
# instead of colliding with an orphaned trellis-run-live session.
LIVE_JOB="$PROJECTS/.trellis-viewer/create-jobs/live"
mkdir -p "$LIVE_JOB" "$PROJECTS/live" "$PROJECTS/live-runtime"
printf '%s\n' '{"create_flow":"prose-program-verification"}' > "$LIVE_JOB/job.json"
printf '%s\n' '{"state":"launch_failed","stage":"verify"}' > "$LIVE_JOB/status.json"
tmux -L "$SOCKET" new-session -d -s trellis-run-live "sleep 60"
delete_output="$(bash "$CREATE" delete live \
  --confirm "$LIVE_JOB" \
  --confirm "$PROJECTS/live" \
  --confirm "$PROJECTS/live-runtime")"
grep -q 'stopping campaign runner tmux session trellis-run-live before delete' \
  <<<"$delete_output"
printf '%s\n' "$delete_output"
[[ ! -e "$LIVE_JOB" && ! -e "$PROJECTS/live" && ! -e "$PROJECTS/live-runtime" ]]
if tmux -L "$SOCKET" list-sessions -F '#S' 2>/dev/null | grep -Fxq trellis-run-live; then
  echo "delete left the campaign runner alive" >&2
  exit 1
fi
echo "runner_session_after_delete=absent"

bash "$CREATE" start live --goal "$TEST_ROOT/GOAL.md" --target "$TEST_ROOT/target.tar.gz"
for _ in $(seq 1 100); do
  state="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"])' "$PROJECTS/.trellis-viewer/create-jobs/live/status.json")"
  [[ "$state" == "done" ]] && break
  [[ "$state" == *_failed ]] && { cat "$PROJECTS/.trellis-viewer/create-jobs/live/create.log"; exit 1; }
  sleep 0.1
done
[[ "$state" == "done" ]]

# A pre-existing output loses before the job claim.
mkdir "$PROJECTS/taken"
if bash "$CREATE" start taken --goal "$TEST_ROOT/GOAL.md" --target "$TEST_ROOT/target.tar.gz" 2>/dev/null; then
  echo "pre-existing campaign was accepted" >&2
  exit 1
fi
[[ ! -e "$PROJECTS/.trellis-viewer/create-jobs/taken" ]]

# Bootstrap's own refusal is preserved in status/log and is never repaired or
# retried over the now-partial campaign.
export TRELLIS_PV_CREATE_BOOTSTRAP_REFUSE=1
bash "$CREATE" start refused --goal "$TEST_ROOT/GOAL.md" --target "$TEST_ROOT/target.tar.gz"
for _ in $(seq 1 100); do
  state="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"])' "$PROJECTS/.trellis-viewer/create-jobs/refused/status.json")"
  [[ "$state" == "build_failed" ]] && break
  sleep 0.1
done
[[ "$state" == "build_failed" ]]
python3 - "$PROJECTS/.trellis-viewer/create-jobs/refused/status.json" <<'PY'
import json, sys
error = json.load(open(sys.argv[1], encoding="utf-8"))["error"]
assert "kernel refusal: Preamble contains an undisclosed declaration" in error, error
PY
grep -q 'kernel refusal: Preamble contains an undisclosed declaration' \
  "$PROJECTS/.trellis-viewer/create-jobs/refused/create.log"
if bash "$CREATE" retry refused 2>/dev/null; then
  echo "fresh-only retry unexpectedly reused a partial campaign" >&2
  exit 1
fi

# `stop` ends a creation that is still running: the create session and every
# Phase-0 lane session on the socket are killed by their exact names, unrelated
# sessions are left alone, and the job is recorded as failed by the operator.
STOP_JOB="$PROJECTS/.trellis-viewer/create-jobs/halted"
mkdir -p "$STOP_JOB"
printf '%s\n' '{"create_flow":"prose-program-verification"}' > "$STOP_JOB/job.json"
printf '%s\n' '{"state":"resolving","stage":"phase0"}' > "$STOP_JOB/status.json"
tmux -L "$SOCKET" new-session -d -s trellis-create-halted "sleep 60"
tmux -L "$SOCKET" new-session -d -s phase0-source_adaptation_worker-worker "sleep 60"
tmux -L "$SOCKET" new-session -d -s phase0-source_adaptation_reviewer-seam-repair-0 "sleep 60"
tmux -L "$SOCKET" new-session -d -s trellis-run-bystander "sleep 60"
stop_output="$(bash "$CREATE" stop halted)"
printf '%s\n' "$stop_output"
grep -q 'stopped 3 tmux session(s)' <<<"$stop_output"
survivors="$(tmux -L "$SOCKET" list-sessions -F '#S' 2>/dev/null || true)"
for name in trellis-create-halted phase0-source_adaptation_worker-worker \
    phase0-source_adaptation_reviewer-seam-repair-0; do
  if grep -Fxq -- "$name" <<<"$survivors"; then
    echo "stop left $name alive" >&2
    exit 1
  fi
done
if ! grep -Fxq -- trellis-run-bystander <<<"$survivors"; then
  echo "stop killed an unrelated tmux session" >&2
  exit 1
fi
python3 - "$STOP_JOB/status.json" <<'STOPPY'
import json, sys
status = json.load(open(sys.argv[1], encoding="utf-8"))
assert status["state"] == "resolve_failed", status
assert status["stage"] == "stopped", status
assert status["error"] == "stopped by operator", status
STOPPY
# Stopping again is a no-op that still exits 0, and delete is no longer blocked.
bash "$CREATE" stop halted >/dev/null
bash "$CREATE" delete halted --confirm "$STOP_JOB" >/dev/null
[[ ! -e "$STOP_JOB" ]]

# A completed creation is a live campaign; stop refuses to touch it.
mkdir -p "$STOP_JOB"
printf '%s\n' '{"create_flow":"prose-program-verification"}' > "$STOP_JOB/job.json"
printf '%s\n' '{"state":"done","stage":"done"}' > "$STOP_JOB/status.json"
if bash "$CREATE" stop halted 2>/dev/null; then
  echo "stop interrupted a completed campaign" >&2
  exit 1
fi
tmux -L "$SOCKET" kill-session -t trellis-run-bystander

# A campaign runner can exhaust its provider before the first liveness poll.
# It still graduates to ordinary run controls; Retry must not rerun genesis.
cat > "$STUBS/paused-runner" <<'SH'
#!/usr/bin/env bash
exec env PYTHONPATH="$TRELLIS_PV_CREATE_FIXTURE_SOURCE" \
  python3 "$TRELLIS_PV_CREATE_FIXTURE_SOURCE/tests/first_launch_fixture.py" "$1" "$2" "$5"
SH
chmod +x "$STUBS/paused-runner"
# Explicitly embed the fixture source; an existing tmux server's environment
# does not inherit new test variables from this caller.
sed -i "s|\$TRELLIS_PV_CREATE_FIXTURE_SOURCE|$ROOT|g" "$STUBS/paused-runner"
export TRELLIS_PV_CREATE_RUNNER="$STUBS/paused-runner"
unset TRELLIS_PV_CREATE_BOOTSTRAP_REFUSE
bash "$CREATE" start budgetpause --goal "$TEST_ROOT/GOAL.md" --target "$TEST_ROOT/target.tar.gz" >/dev/null
PAUSED_JOB="$PROJECTS/.trellis-viewer/create-jobs/budgetpause"
for _ in $(seq 1 100); do
  state="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"])' "$PAUSED_JOB/status.json")"
  [[ "$state" == done ]] && break
  [[ "$state" == *_failed ]] && { cat "$PAUSED_JOB/create.log"; exit 1; }
  sleep 0.2
done
[[ "$state" == done && ! -e "$PROJECTS/budgetpause/.trellis-creating" ]]
for _ in $(seq 1 50); do
  [[ -f "$PROJECTS/budgetpause-runtime/pause_request.json" ]] && break
  sleep 0.1
done
[[ -f "$PROJECTS/budgetpause-runtime/pause_request.json" ]]
cp "$PROJECTS/budgetpause-runtime/protocol_state.json" "$TEST_ROOT/paused-state"
# Interrupted graduation is adopted by Retry; a second Retry is terminal.
printf '%s\n' '{"state":"launch_failed","stage":"verify"}' > "$PAUSED_JOB/status.json"
touch "$PROJECTS/budgetpause/.trellis-creating"
bash "$CREATE" retry budgetpause >/dev/null
[[ ! -e "$PROJECTS/budgetpause/.trellis-creating" ]]
if bash "$CREATE" retry budgetpause 2>/dev/null; then exit 1; fi
cmp "$TEST_ROOT/paused-state" "$PROJECTS/budgetpause-runtime/protocol_state.json"

echo "pv create run tests: ok"
