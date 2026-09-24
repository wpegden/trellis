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
  # Honor a prebuilt kernel binary named by TRELLIS_TRELLIS_KERNEL_CMD (the
  # same env the Python bridge/kernel_cli resolve against). This lets the
  # long-lived step loop run the release binary instead of a debug
  # `cargo run`, matching the acceptance-subprocess profile. Unset → the
  # original debug `cargo run` path, so other runs are unaffected.
  if [[ -n "${TRELLIS_TRELLIS_KERNEL_CMD:-}" ]]; then
    TRELLIS_RUNTIME_BRIDGE_CMD="$BRIDGE_CMD" \
    TRELLIS_RUNTIME_CHECKPOINT_HOOK="$CHECKPOINT_HOOK" \
    $TRELLIS_TRELLIS_KERNEL_CMD
  else
    TRELLIS_RUNTIME_BRIDGE_CMD="$BRIDGE_CMD" \
    TRELLIS_RUNTIME_CHECKPOINT_HOOK="$CHECKPOINT_HOOK" \
    cargo run --quiet --manifest-path "$RUNTIME_MANIFEST" --bin trellis_runtime_cli
  fi
}

# Record how this run was launched, so a stopped run can be resumed exactly
# as it was. A checkpoint stop leaves no process behind, so the environment
# is otherwise unrecoverable: it lives only in the dead process. Restarting
# with a merely-plausible env is not a cosmetic difference — a missing
# TRELLIS_CSC_LAST_CLEAN_THRESHOLD or v2_strict flag has previously cost a
# multi-hundred-cycle forced rewind, because the kernel came up unable to
# reproduce the fingerprints it had recorded.
#
# Allowlisted by prefix rather than captured wholesale: the supervisor env
# holds provider credentials, and this file is written into the runtime root
# where the viewer and operator tooling read it. Anything that looks like a
# secret is dropped. The checker token is deliberately among them — it is
# minted fresh on every launch a few lines below, so replaying a stale one
# would be both useless and wrong.
capture_launch_env() {
  local runtime_root="$1"
  local max_steps="${2:-}"
  local tmux_session=""
  if [[ -n "${TMUX:-}" ]] && command -v tmux >/dev/null 2>&1; then
    tmux_session="$(tmux -L trellis display-message -p '#S' 2>/dev/null || true)"
  fi
  local trellis_head=""
  trellis_head="$(git -C "$ROOT_DIR" rev-parse HEAD 2>/dev/null || true)"
  TRELLIS_LAUNCH_TMUX_SESSION="$tmux_session" \
  TRELLIS_LAUNCH_MAX_STEPS="$max_steps" \
  TRELLIS_LAUNCH_ROOT_DIR="$ROOT_DIR" \
  TRELLIS_LAUNCH_TRELLIS_HEAD="$trellis_head" \
  python3 - "$runtime_root" <<'PY' || true
import json, os, sys, time
from pathlib import Path

runtime_root = Path(sys.argv[1])
if not runtime_root.is_dir():
    sys.exit(0)

KEEP_PREFIXES = ("TRELLIS_", "LEAN_", "LAKE_", "ELAN_", "RUST_", "CARGO_")
KEEP_EXACT = {"PATH", "HOME", "LANG", "LC_ALL", "SHELL", "USER", "LOGNAME", "PYTHONPATH"}
SECRET_MARKERS = ("TOKEN", "KEY", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL", "AUTH")

env = {}
for name, value in os.environ.items():
    if name.startswith("TRELLIS_LAUNCH_"):
        continue
    if any(marker in name.upper() for marker in SECRET_MARKERS):
        continue
    if name in KEEP_EXACT or name.startswith(KEEP_PREFIXES):
        env[name] = value

max_steps = os.environ.get("TRELLIS_LAUNCH_MAX_STEPS") or None
root_dir = os.environ.get("TRELLIS_LAUNCH_ROOT_DIR") or ""
payload = {
    "captured_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    "captured_at_epoch": int(time.time()),
    "runtime_root": str(runtime_root),
    "trellis_root": root_dir,
    "trellis_sh": os.path.join(root_dir, "scripts", "trellis.sh") if root_dir else "",
    "max_steps": max_steps,
    # The source HEAD the supervisor binary was built from. Resume compares
    # against it: coming back up on a tree that has moved on makes the
    # startup fingerprint-observe come up empty, which the kernel reads as
    # "state diverges from disk" and answers with an auto-rewind.
    "trellis_head": os.environ.get("TRELLIS_LAUNCH_TRELLIS_HEAD") or None,
    "cwd": os.getcwd(),
    "tmux_socket": "trellis",
    "tmux_session": os.environ.get("TRELLIS_LAUNCH_TMUX_SESSION") or None,
    "env": env,
}
target = runtime_root / "launch_env.json"
tmp = target.with_suffix(".json.tmp")
tmp.write_text(json.dumps(payload, indent=2, sort_keys=True), encoding="utf-8")
tmp.replace(target)
PY
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
    # After every export the supervisor depends on, so the snapshot is what
    # the kernel actually ran with rather than what the caller happened to set.
    capture_launch_env "$runtime_root" "$max_steps"
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
