#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_MANIFEST="$ROOT_DIR/kernel/Cargo.toml"
BRIDGE_CMD="${TRELLIS_RUNTIME_BRIDGE_CMD:-$ROOT_DIR/scripts/trellis_bridge.sh}"
CHECKPOINT_HOOK="${TRELLIS_RUNTIME_CHECKPOINT_HOOK:-$ROOT_DIR/scripts/trellis_git_checkpoint_hook.sh}"

usage() {
  cat <<'EOF'
Usage:
  scripts/trellis.sh import-legacy <config_path> <runtime_root> [state_path] [tablet_path]
  scripts/trellis.sh init <config_path> <runtime_root>
  scripts/trellis.sh show <runtime_root>
  scripts/trellis.sh preview <runtime_root>
  scripts/trellis.sh step <runtime_root>
  scripts/trellis.sh run <runtime_root> [max_steps]
  scripts/trellis.sh report <runtime_root>
EOF
}

runtime_cli() {
  if ! command -v cargo >/dev/null 2>&1 && [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1090
    source "$HOME/.cargo/env"
  fi
  TRELLIS_RUNTIME_BRIDGE_CMD="$BRIDGE_CMD" \
  TRELLIS_RUNTIME_CHECKPOINT_HOOK="$CHECKPOINT_HOOK" \
  cargo run --quiet --manifest-path "$RUNTIME_MANIFEST" --bin trellis_runtime_cli
}

materialize_runtime_support_from_config() {
  local config_path="$1"
  PYTHONPATH="$ROOT_DIR${PYTHONPATH:+:$PYTHONPATH}" python3 - "$config_path" <<'PY'
from pathlib import Path
import sys

from trellis.checking import write_scripts
from trellis.config import load_config

config = load_config(Path(sys.argv[1]).resolve())
write_scripts(config.repo_path, config.state_dir)
PY
}

action="${1:-}"
case "$action" in
  import-legacy)
    config_path="${2:-}"
    runtime_root="${3:-}"
    state_path="${4:-}"
    tablet_path="${5:-}"
    if [[ -z "$config_path" || -z "$runtime_root" ]]; then
      usage
      exit 2
    fi
    if [[ -n "$state_path" && -n "$tablet_path" ]]; then
      runtime_cli <<EOF
{
  "action": "import_legacy",
  "root": "$runtime_root",
  "config_path": "$(cd "$(dirname "$config_path")" && pwd)/$(basename "$config_path")",
  "state_path": "$(cd "$(dirname "$state_path")" && pwd)/$(basename "$state_path")",
  "tablet_path": "$(cd "$(dirname "$tablet_path")" && pwd)/$(basename "$tablet_path")"
}
EOF
    else
      runtime_cli <<EOF
{
  "action": "import_legacy",
  "root": "$runtime_root",
  "config_path": "$(cd "$(dirname "$config_path")" && pwd)/$(basename "$config_path")"
}
EOF
    fi
    materialize_runtime_support_from_config "$config_path"
    ;;
  init)
    config_path="${2:-}"
    runtime_root="${3:-}"
    if [[ -z "$config_path" || -z "$runtime_root" ]]; then
      usage
      exit 2
    fi
    materialize_runtime_support_from_config "$config_path"
    runtime_cli <<EOF
{
  "action": "init_from_config",
  "root": "$runtime_root",
  "config_path": "$(cd "$(dirname "$config_path")" && pwd)/$(basename "$config_path")"
}
EOF
    ;;
  show)
    runtime_root="${2:-}"
    if [[ -z "$runtime_root" ]]; then
      usage
      exit 2
    fi
    runtime_cli <<EOF
{
  "action": "show",
  "root": "$runtime_root"
}
EOF
    ;;
  preview)
    runtime_root="${2:-}"
    if [[ -z "$runtime_root" ]]; then
      usage
      exit 2
    fi
    if ! command -v cargo >/dev/null 2>&1 && [[ -f "$HOME/.cargo/env" ]]; then
      # shellcheck disable=SC1090
      source "$HOME/.cargo/env"
    fi
    python3 - "$runtime_root" "$BRIDGE_CMD" "$RUNTIME_MANIFEST" <<'PY'
import json
import os
import subprocess
import sys
from pathlib import Path

runtime_root = Path(sys.argv[1]).resolve()
bridge_cmd = Path(sys.argv[2]).resolve()
runtime_manifest = Path(sys.argv[3]).resolve()
runtime_output = subprocess.run(
    [
        "cargo",
        "run",
        "--quiet",
        "--manifest-path",
        str(runtime_manifest),
        "--bin",
        "trellis_runtime_cli",
    ],
    input=json.dumps({"action": "current_request", "root": str(runtime_root)}),
    text=True,
    capture_output=True,
    check=True,
)
response = json.loads(runtime_output.stdout)
if response.get("status") != "current_request_ok":
    raise SystemExit(response.get("message") or "failed to load current request")
request = response.get("request")
if not isinstance(request, dict):
    raise SystemExit("runtime did not return a request payload")
metadata = response.get("metadata")
if not isinstance(metadata, dict):
    raise SystemExit("runtime did not return metadata")
config_path = str(metadata.get("config_path", "") or "").strip()
if not config_path:
    raise SystemExit("runtime metadata is missing config_path")
payload = {
    "config_path": config_path,
    "runtime_root": str(runtime_root),
    "request": request,
}
env = dict(os.environ)
env["TRELLIS_TRELLIS_BRIDGE_DRY_RUN"] = "1"
subprocess.run(
    [str(bridge_cmd)],
    input=json.dumps(payload),
    text=True,
    env=env,
    check=True,
)
PY
    ;;
  step)
    runtime_root="${2:-}"
    if [[ -z "$runtime_root" ]]; then
      usage
      exit 2
    fi
    runtime_cli <<EOF
{
  "action": "step",
  "root": "$runtime_root"
}
EOF
    ;;
  run)
    runtime_root="${2:-}"
    max_steps="${3:-}"
    if [[ -z "$runtime_root" ]]; then
      usage
      exit 2
    fi
    # Reviewer source-recourse snapshot. The reviewer's bwrap mounts this
    # read-only so the reviewer can consult kernel + Python source as a
    # fallback when process semantics seem to block progress. Defaults to
    # HEAD of the trellis source tree; override with
    # TRELLIS_REVIEWER_SOURCE_SHA=<sha> in the supervisor env to pin a
    # different commit. The snapshot is taken once per run at startup so
    # the reviewer reads what was true at that SHA, not whatever the
    # live tree happens to contain right now.
    source_sha="${TRELLIS_REVIEWER_SOURCE_SHA:-}"
    if [[ -z "$source_sha" ]]; then
      if ! source_sha=$(git -C "$ROOT_DIR" rev-parse HEAD 2>/dev/null); then
        source_sha=""
      fi
    fi
    if [[ -n "$source_sha" ]]; then
      snapshot_dir="$runtime_root/trellis-source-snapshot/$source_sha"
      if [[ ! -d "$snapshot_dir" ]]; then
        if ! mkdir -p "$snapshot_dir" 2>/dev/null; then
          source_sha=""
        elif ! git -C "$ROOT_DIR" archive "$source_sha" 2>/dev/null | tar -x -C "$snapshot_dir" 2>/dev/null; then
          # If archive fails (e.g. shallow repo), fall back to no snapshot —
          # reviewer just won't have source access this run.
          rm -rf "$snapshot_dir"
          source_sha=""
        fi
      fi
    fi
    if [[ -n "$source_sha" ]]; then
      export TRELLIS_REVIEWER_SOURCE_SNAPSHOT="$snapshot_dir"
      export TRELLIS_REVIEWER_SOURCE_SHA="$source_sha"
    fi
    # Bwrap-only-migration: mint + register a supervisor token so
    # supervisor-side check.py invocations (prepare_compiled_support,
    # lean_compile_node, etc.) pass the checker's per-request token
    # gate. Bursts continue to mint their own per-burst tokens via
    # the bridge; this registers a stable supervisor-lifetime token
    # in addition.
    TRELLIS_CHECKER_TOKEN="$(PYTHONPATH="$ROOT_DIR${PYTHONPATH:+:$PYTHONPATH}" python3 -c "
import os, sys
from pathlib import Path
from trellis.runtime.bridge import _mint_burst_token, _register_burst_token
token = _mint_burst_token()
_register_burst_token(
    Path('$runtime_root'),
    token=token,
    burst_id='supervisor',
    kind='supervisor',
    request_id=0,
    cycle=0,
)
sys.stdout.write(token)
")"
    export TRELLIS_CHECKER_TOKEN
    if [[ -n "$max_steps" ]]; then
      runtime_cli <<EOF
{
  "action": "run",
  "root": "$runtime_root",
  "max_steps": $max_steps
}
EOF
    else
      runtime_cli <<EOF
{
  "action": "run",
  "root": "$runtime_root"
}
EOF
    fi
    ;;
  report)
    runtime_root="${2:-}"
    if [[ -z "$runtime_root" ]]; then
      usage
      exit 2
    fi
    PYTHONPATH="$ROOT_DIR${PYTHONPATH:+:$PYTHONPATH}" \
      python3 -m trellis.usage_report "$runtime_root"
    ;;
  *)
    usage
    exit 2
    ;;
esac
