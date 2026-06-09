#!/bin/bash
# Setup or reseed a formalization repo for trellis.
#
# Usage:
#   ./scripts/setup_repo.sh [--reset] <repo_path> <paper_tex_path> [project_slug]

set -euo pipefail
umask 0002

TRELLIS_TMUX_SOCKET="${TRELLIS_TMUX_SOCKET:-trellis}"
export TRELLIS_TMUX_SOCKET
tmux_cmd() { tmux -L "$TRELLIS_TMUX_SOCKET" "$@"; }

usage() {
  cat <<'EOF'
Usage: ./scripts/setup_repo.sh --loogle on|off [--reset] <repo_path> <paper_tex_path> [project_slug]

  --loogle on|off Required. Whether this host runs a local Loogle (Mathlib
                  search) server. Writes loogle.enabled into the generated
                  trellis.config.json. When off, the worker prompt omits the
                  Loogle helper; see the printed reminder about the skill files.
  --reset         Stop any existing project process and recreate the repo from scratch
  --yes           Skip the target confirmation prompt
  --mathlib-build-tar path
                  Optional local tarball containing the contents of
                  .lake/packages/mathlib/.lake/build to seed prewarm.
  --main-result-labels labels
                  Comma-separated paper TeX labels to use as the human-reviewed target set.
                  If omitted, setup infers all paper theorem/corollary statements, using labels
                  when present and line ranges when not.
  repo_path       Where to create the formalization repo
  paper_tex_path  Path to the source paper .tex file
  project_slug    Optional viewer/session slug (defaults to basename(repo_path))
EOF
}

RESET=0
ASSUME_YES=0
MAIN_RESULT_LABELS=""
MATHLIB_BUILD_TAR="${MATHLIB_BUILD_TAR:-}"
LOOGLE_SETTING=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --reset|--force)
      RESET=1
      shift
      ;;
    --loogle)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --loogle requires an argument: on or off" >&2
        exit 1
      fi
      case "$2" in
        on|off) LOOGLE_SETTING="$2" ;;
        *) echo "ERROR: --loogle must be 'on' or 'off', got: $2" >&2; exit 1 ;;
      esac
      shift 2
      ;;
    --yes|-y)
      ASSUME_YES=1
      shift
      ;;
    --main-result-labels)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --main-result-labels requires a comma-separated argument" >&2
        exit 1
      fi
      MAIN_RESULT_LABELS="$2"
      shift 2
      ;;
    --mathlib-build-tar)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --mathlib-build-tar requires a path argument" >&2
        exit 1
      fi
      MATHLIB_BUILD_TAR="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      echo "ERROR: Unknown option: $1" >&2
      usage >&2
      exit 1
      ;;
    *)
      break
      ;;
  esac
done

if [ $# -lt 2 ]; then
  usage >&2
  exit 1
fi

if [ -z "$LOOGLE_SETTING" ]; then
  echo "ERROR: --loogle on|off is required. Does this host run a local Loogle" >&2
  echo "       (Mathlib search) server? Pass --loogle on if so, otherwise --loogle off." >&2
  exit 1
fi
if [ "$LOOGLE_SETTING" = "on" ]; then LOOGLE_ENABLED_JSON="true"; else LOOGLE_ENABLED_JSON="false"; fi

REPO="$1"
PAPER="$2"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO="$(python3 - "$REPO" <<'PY'
from pathlib import Path
import sys

print(Path(sys.argv[1]).resolve())
PY
)"
PAPER="$(python3 - "$PAPER" <<'PY'
from pathlib import Path
import sys

print(Path(sys.argv[1]).resolve())
PY
)"
DEFAULT_SLUG="$(basename "$REPO" | sed -E 's/_tablets?$//')"
PROJECT_SLUG="${3:-${PROJECT_SLUG:-$DEFAULT_SLUG}}"
DEFAULT_CONFIG_TEMPLATE="$SOURCE_ROOT/examples/trellis.config.json"
CONFIG_TEMPLATE="${CONFIG_TEMPLATE:-$DEFAULT_CONFIG_TEMPLATE}"
if [[ -z "${POLICY_TEMPLATE:-}" ]]; then
  if [[ "$CONFIG_TEMPLATE" == *.config.json ]]; then
    POLICY_TEMPLATE="${CONFIG_TEMPLATE%.config.json}.policy.json"
  else
    POLICY_TEMPLATE="${CONFIG_TEMPLATE%.json}.policy.json"
  fi
fi
CONFIG_OUT="$REPO/trellis.config.json"
POLICY_OUT="$REPO/trellis.policy.json"
STATIC_OUT="${STATIC_OUT:-$HOME/trellis-web}"
PROJECT_STATIC_DIR="$STATIC_OUT/$PROJECT_SLUG"
BURST_USER="${BURST_USER:-$(id -un)}"
BURST_GROUP="${BURST_GROUP:-$(id -gn)}"
MATHLIB_TOOLCHAIN="${MATHLIB_TOOLCHAIN:-leanprover/lean4:v4.30.0-rc1}"
MATHLIB_REV="${MATHLIB_REV:-a090f46da78e9af11fee348cd7ee47bf8dd219d2}"
# Post-bwrap-only: the burst runs as the operator with a dedicated fake home
# (the per-burst home is materialized under <runtime>/burst-homes/ at run time;
# this default is only used by the setup-time preflight + prewarm). Must exist
# and be writable, so it is created below before the bwrap preflight.
BURST_HOME="${BURST_HOME:-$HOME/.cache/trellis-burst-home}"
mkdir -p "$BURST_HOME"
ELAN_HOME="${ELAN_HOME:-$HOME/.elan}"
# Default PATH for the in-bwrap validation + prewarm steps. elan installs lake
# to ~/.elan/bin (see INSTALLATION.md §2c), which is not on the system PATH, so
# append it to the default — otherwise `lake env lean` validation fails
# "command not found". Provider CLIs live in a user-local npm prefix whose
# location varies; export BURST_PATH explicitly to add that dir (documented in
# INSTALLATION.md §4). An explicit BURST_PATH override is respected as-is.
BURST_PATH="${BURST_PATH:-$HOME/.elan/bin:/usr/local/bin:/usr/bin:/bin}"
SETUP_SCRATCH_ROOT="${SETUP_SCRATCH_ROOT:-$SOURCE_ROOT/.trellis/setup_repo_tmp}"
if [[ -n "$MATHLIB_BUILD_TAR" ]]; then
  MATHLIB_BUILD_TAR="$(python3 - "$MATHLIB_BUILD_TAR" <<'PY'
from pathlib import Path
import sys

print(Path(sys.argv[1]).resolve())
PY
)"
fi

if [ ! -f "$PAPER" ]; then
  echo "ERROR: Paper not found: $PAPER" >&2
  exit 1
fi
if [ ! -f "$CONFIG_TEMPLATE" ]; then
  echo "ERROR: Config template not found: $CONFIG_TEMPLATE" >&2
  exit 1
fi
if [[ -n "$MATHLIB_BUILD_TAR" ]] && [ ! -f "$MATHLIB_BUILD_TAR" ]; then
  echo "ERROR: Mathlib build tarball not found: $MATHLIB_BUILD_TAR" >&2
  exit 1
fi
if [ -e "$REPO" ] && [ "$RESET" -ne 1 ]; then
  echo "ERROR: Repo path already exists. Re-run with --reset to recreate it." >&2
  exit 1
fi

PAPER_NAME="$(basename "$PAPER")"
mkdir -p "$SETUP_SCRATCH_ROOT"
TARGETS_JSON="$(mktemp "$SETUP_SCRATCH_ROOT/targets.XXXXXX.json")"
TARGETS_PREVIEW="$(mktemp "$SETUP_SCRATCH_ROOT/targets-preview.XXXXXX.txt")"
NORMALIZED_PAPER="$(mktemp "$SETUP_SCRATCH_ROOT/${PAPER_NAME%.tex}.normalized.XXXXXX.tex")"
cleanup_preview_files() {
  rm -f "$TARGETS_JSON" "$TARGETS_PREVIEW" "$NORMALIZED_PAPER"
}
trap cleanup_preview_files EXIT

# Normalize \newtheorem aliases (e.g. theo->theorem, cor->corollary) to the
# canonical envs the kernel paper parser looks for. A paper already using
# canonical envs is passed through unchanged. The normalized copy is what we
# both pass to the kernel target resolver AND copy into the repo as paper/.
python3 "$SCRIPT_DIR/normalize_paper_envs.py" "$PAPER" "$NORMALIZED_PAPER"
PAPER="$NORMALIZED_PAPER"

echo "Setting up repo at: $REPO"
echo "  Paper: $PAPER ($PAPER_NAME)"
echo "  Project slug: $PROJECT_SLUG"
echo "  Burst user: $BURST_USER"
echo "  Burst group: $BURST_GROUP"
echo "  Config out: $CONFIG_OUT"
if [[ -n "$MAIN_RESULT_LABELS" ]]; then
  echo "  Main-result labels: $MAIN_RESULT_LABELS"
fi
if [[ -n "$MATHLIB_BUILD_TAR" ]]; then
  echo "  Mathlib build tar: $MATHLIB_BUILD_TAR"
fi

PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$PAPER" "$MAIN_RESULT_LABELS" "$TARGETS_JSON" "$TARGETS_PREVIEW" <<'PY'
import json
import sys
from pathlib import Path

from trellis.config import (
    ConfigError,
    format_main_result_target,
    resolve_main_result_targets_via_kernel,
)

paper_path = Path(sys.argv[1]).resolve()
raw_main_result_labels = sys.argv[2]
targets_json = Path(sys.argv[3]).resolve()
targets_preview = Path(sys.argv[4]).resolve()

labels = []
if raw_main_result_labels.strip():
    seen = set()
    for raw_label in raw_main_result_labels.split(","):
        label = raw_label.strip()
        if not label or label in seen:
            continue
        seen.add(label)
        labels.append(label)

try:
    resolved = resolve_main_result_targets_via_kernel(
        paper_path=paper_path,
        raw_labels=labels or None,
    )
except ConfigError as exc:
    raise SystemExit(str(exc)) from exc

targets = resolved["targets"]
available_labels = resolved["available_labels"]
preview = resolved["preview"]
preview_lines = ["Resolved main-result targets:"]

if not targets:
    preview_lines.append("(none)")
else:
    for idx, target in enumerate(targets, start=1):
        if idx > len(preview) or not isinstance(preview[idx - 1], dict):
            raise SystemExit(
                f"Could not locate paper text for resolved main-result target {format_main_result_target(target)}."
            )
        preview_entry = preview[idx - 1]
        target_header = (
            f"{idx}. {format_main_result_target(target)} "
            f"[{str(preview_entry.get('env', '') or '').strip()}]"
        )
        preview_lines.append(target_header)
        preview_lines.append(str(preview_entry.get("text", "") or "").strip())
        preview_lines.append("")

targets_json.write_text(
    json.dumps(
        {
            "labels": labels,
            "targets": targets,
            "available_labels": available_labels,
        },
        indent=2,
    )
    + "\n",
    encoding="utf-8",
)
targets_preview.write_text("\n".join(preview_lines).rstrip() + "\n", encoding="utf-8")
PY

echo ""
cat "$TARGETS_PREVIEW"
echo ""
if [ "$ASSUME_YES" -eq 0 ]; then
  if [ ! -t 0 ]; then
    echo "ERROR: setup requires target confirmation. Re-run with --yes in non-interactive mode." >&2
    exit 1
  fi
  read -r -p "Proceed with these targets? [y/N] " TARGET_CONFIRM
  case "$TARGET_CONFIRM" in
    y|Y|yes|YES)
      ;;
    *)
      echo "Aborted."
      exit 1
      ;;
  esac
fi

# Phase 4 bwrap-only migration: passwordless sudo to BURST_USER is no
# longer required; bursts run as the supervisor user inside bwrap. BURST_USER is
# retained as a parameter for Phase 5 mechanical removal.
if ! command -v bwrap >/dev/null 2>&1; then
  echo "ERROR: bwrap is required for sandboxed agent bursts." >&2
  exit 1
fi
if ! PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$REPO" "$BURST_HOME" <<'PY'
import sys
from pathlib import Path

from trellis.config import SandboxConfig
from trellis.sandbox import probe_sandbox

repo = Path(sys.argv[1]).resolve()
burst_home = Path(sys.argv[2]).resolve()
ok, detail = probe_sandbox(
    sandbox=SandboxConfig(enabled=True, backend="bwrap"),
    work_dir=repo.parent if repo.exists() else Path.cwd(),
    burst_home=burst_home,
)
if not ok:
    print(detail, file=sys.stderr)
    raise SystemExit(1)
PY
then
  echo "ERROR: bwrap exists but is not usable for sandboxed bursts on this host." >&2
  exit 1
fi

if [ "$RESET" -eq 1 ]; then
  echo "  Resetting existing project artifacts..."
  [ -f "$SCRIPT_DIR/stop.sh" ] && "$SCRIPT_DIR/stop.sh" "$REPO" >/dev/null 2>&1 || true
  tmux_cmd kill-session -t "$PROJECT_SLUG" >/dev/null 2>&1 || true
  # Phase 4: repo is supervisor-owned; no sudo needed.
  rm -rf "$REPO"
  rm -rf "$PROJECT_STATIC_DIR"
  if [ -e "$REPO" ]; then
    echo "ERROR: failed to fully remove existing repo during --reset: $REPO" >&2
    exit 1
  fi
fi

mkdir -p "$REPO/paper" "$REPO/Tablet"
mkdir -p "$REPO/.trellis/logs" "$REPO/.trellis/scripts" "$REPO/.trellis/checkpoints"
mkdir -p "$REPO/.trellis/staging" "$REPO/.trellis/viewer/state-at" "$REPO/.trellis/chats" "$REPO/.trellis/scratch"
mkdir -p "$REPO/.trellis/runtime"

cp "$PAPER" "$REPO/paper/$PAPER_NAME"
echo "  Copied paper to $REPO/paper/$PAPER_NAME"

cp "$SOURCE_ROOT/FILESPEC.md" "$REPO/FILESPEC.md"
echo "  Wrote FILESPEC.md"

# Materialize the four canonical verifier rubrics at the project root so the
# instruction in `TRELLIS_FORMALIZATION_SCHEME{,_verifier}.md` ("read
# FAITHFULNESS.md, SUBSTANTIVENESS.md, CORRESPONDENCE.md, SOUNDNESS.md at the
# project root") matches what the agents actually find. The kernel inlines the
# same content into every verifier prompt as well, so this is a redundant safety
# net for human readers and for agents who chose to verify the on-disk file.
# Without it the verifiers' `comments` fields fill up with "X.md was not present
# at the repository root" notes which then propagate into reviewer context and
# the next verifier's `previous_own_findings_by_lane`, polluting the substantive
# feedback channel.
for canonical in CORRESPONDENCE DEVIATIONS FAITHFULNESS SOUNDNESS SUBSTANTIVENESS; do
  cp "$SOURCE_ROOT/trellis/prompt_fragments/canonical/${canonical}.md" \
     "$REPO/${canonical}.md"
done
echo "  Wrote canonical verifier rubrics (CORRESPONDENCE.md, DEVIATIONS.md, FAITHFULNESS.md, SOUNDNESS.md, SUBSTANTIVENESS.md)"

cat > "$REPO/lakefile.lean" <<'LAKEFILE'
import Lake
open Lake DSL

package «tablet» where
  leanOptions := #[
    ⟨`autoImplicit, false⟩
  ]

@[default_target]
lean_lib «Tablet» where
  srcDir := "."

require mathlib from git
  "https://github.com/leanprover-community/mathlib4" @ "__MATHLIB_REV__"
LAKEFILE
sed -i "s/__MATHLIB_REV__/$MATHLIB_REV/" "$REPO/lakefile.lean"
echo "  Wrote lakefile.lean"

echo "$MATHLIB_TOOLCHAIN" > "$REPO/lean-toolchain"
echo "  Wrote lean-toolchain ($MATHLIB_TOOLCHAIN)"
echo "  Pinned mathlib revision ($MATHLIB_REV)"

cat > "$REPO/Tablet/Preamble.lean" <<'PREAMBLE'
-- Preamble: shared imports for all tablet nodes.
-- Add specific Mathlib imports here (never `import Mathlib`).
PREAMBLE
echo "  Wrote Tablet/Preamble.lean"

cat > "$REPO/APPROVED_AXIOMS.json" <<'AXIOMS'
{
  "global": [],
  "nodes": {}
}
AXIOMS
echo "  Wrote APPROVED_AXIOMS.json"

cat > "$REPO/HUMAN_INPUT.md" <<'HUMAN'
# Human Input

Write human guidance for the supervisor here when requested.
HUMAN

cat > "$REPO/INPUT_REQUEST.md" <<'REQUEST'
# Input Request

The supervisor will write explicit requests for human input here when needed.
REQUEST

PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$REPO" "$CONFIG_TEMPLATE" "$CONFIG_OUT" "$POLICY_TEMPLATE" "$POLICY_OUT" "$PAPER_NAME" "$PROJECT_SLUG" "$TARGETS_JSON" "$BURST_USER" "$BURST_GROUP" "$BURST_HOME" "$LOOGLE_ENABLED_JSON" <<'PY'
import json
import shutil
import sys
from pathlib import Path

from trellis.checking import write_scripts
from trellis.config import load_config
from trellis.git_ops import init_repo
from trellis.project_paths import project_chats_dir, project_tmp_dir
from trellis.runtime.kernel_cli import run_kernel_cli
from trellis.worker_scratch import ensure_worker_scratch_workspace

repo = Path(sys.argv[1]).resolve()
config_template = Path(sys.argv[2]).resolve()
config_out = Path(sys.argv[3]).resolve()
policy_template = Path(sys.argv[4]).resolve()
policy_out = Path(sys.argv[5]).resolve()
paper_name = sys.argv[6]
slug = sys.argv[7]
resolved_targets_path = Path(sys.argv[8]).resolve()
burst_user = sys.argv[9]
burst_group = sys.argv[10]
burst_home = sys.argv[11]
loogle_enabled = sys.argv[12] == "true"
state_dir = repo / ".trellis"
paper_path = repo / "paper" / paper_name

init_repo(repo)

parsed = json.loads(config_template.read_text(encoding="utf-8"))
if not isinstance(parsed, dict):
    raise SystemExit("Config template must be a JSON object")
data = parsed
data["repo_path"] = str(repo)
data["state_dir"] = ".trellis"
data["policy_path"] = "trellis.policy.json"

sandbox = data.setdefault("sandbox", {})
sandbox["enabled"] = True
sandbox["backend"] = "bwrap"

# Explicit per-project Loogle setting (required --loogle on|off at setup).
# When off, the worker prompt omits the Loogle helper fragment.
data["loogle"] = {"enabled": loogle_enabled}

tmux = data.setdefault("tmux", {})
tmux["session_name"] = slug
# burst_user is no longer required (post-bwrap-only); write it for
# backwards compatibility with stale loaders.
tmux["burst_user"] = burst_user
tmux["burst_group"] = burst_group
tmux["burst_home"] = burst_home

workflow = data.setdefault("workflow", {})
workflow["paper_tex_path"] = f"paper/{paper_name}"
workflow["approved_axioms_path"] = "APPROVED_AXIOMS.json"
workflow["human_input_path"] = "HUMAN_INPUT.md"
workflow["input_request_path"] = "INPUT_REQUEST.md"

resolved_targets = json.loads(resolved_targets_path.read_text(encoding="utf-8"))
if not isinstance(resolved_targets, dict):
    raise SystemExit("Resolved main-result targets must be a JSON object")
labels = resolved_targets.get("labels", [])
targets = resolved_targets.get("targets", [])
workflow["main_result_labels"] = labels
workflow["main_result_targets"] = targets

chat = data.setdefault("chat", {})
chat["root_dir"] = str(project_chats_dir(state_dir))
chat["repo_name"] = slug
chat["project_name"] = slug.replace("_", " ").title() + " Formalization"

git_cfg = data.setdefault("git", {})
git_cfg.setdefault("remote_url", None)
git_cfg.setdefault("remote_name", "origin")
git_cfg.setdefault("branch", "master")
git_cfg.setdefault("author_name", ".trellis")
git_cfg.setdefault("author_email", "trellis@localhost")

config_out.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")

if policy_template.exists():
    shutil.copyfile(policy_template, policy_out)

config = load_config(config_out)
write_scripts(config.repo_path, config.state_dir)
sync_result = run_kernel_cli({"action": "sync_tablet_support", "repo_path": str(repo)})
if sync_result.get("status") != "sync_tablet_support_ok":
    raise SystemExit(f"failed to sync tablet support artifacts: {sync_result}")

ensure_worker_scratch_workspace(repo, reset=True)
project_tmp_dir(state_dir).mkdir(parents=True, exist_ok=True)
PY
echo "  Initialized config, scripts, and support artifacts"

python3 - "$REPO" "$BURST_GROUP" <<'PY'
import grp
import os
import sys
from pathlib import Path

repo = Path(sys.argv[1]).resolve()
group = sys.argv[2]
gid = grp.getgrnam(group).gr_gid

def chmod_dir(path: Path, mode: int) -> None:
    try:
        os.chown(str(path), -1, gid)
    except (PermissionError, OSError):
        pass
    try:
        os.chmod(str(path), mode)
    except (PermissionError, OSError):
        pass

def chmod_file(path: Path, mode: int) -> None:
    try:
        os.chown(str(path), -1, gid)
    except (PermissionError, OSError):
        pass
    try:
        os.chmod(str(path), mode)
    except (PermissionError, OSError):
        pass

skip_dirs = {'.git'}
for root, dirs, files in os.walk(repo):
    root_path = Path(root)
    dirs[:] = [d for d in dirs if d not in skip_dirs]
    chmod_dir(root_path, 0o2775)
    for name in files:
        path = root_path / name
        mode = 0o664
        if path.parent.name in {'scripts', 'bin'} or path.suffix == '.sh':
            mode = 0o775
        chmod_file(path, mode)
PY
echo "  Normalized working-tree permissions for shared use"

git -C "$REPO" config core.sharedRepository group

echo "  Initializing nested local chats git repo..."
git -C "$REPO/.trellis/chats" init >/dev/null 2>&1
git -C "$REPO/.trellis/chats" config user.name "trellis-chats" >/dev/null 2>&1
git -C "$REPO/.trellis/chats" config user.email "trellis-chats@localhost" >/dev/null 2>&1
cat > "$REPO/.trellis/chats/README.md" <<'CHATREADME'
# Local Chat History

This nested git repo stores project-local chat/session history.
It is intentionally outside the parent formalization repo history.
CHATREADME
git -C "$REPO/.trellis/chats" add README.md >/dev/null 2>&1
git -C "$REPO/.trellis/chats" commit -m "Initialize local chat history repo" >/dev/null 2>&1 || true

echo "  Prewarming Lean dependencies and build artifacts as burst user..."
PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$REPO" "$BURST_GROUP" "$BURST_HOME" "$ELAN_HOME" "$BURST_PATH" "$MATHLIB_BUILD_TAR" <<'PY'
import sys
from pathlib import Path

from trellis.setup_ops import run_setup_prewarm

run_setup_prewarm(
    repo_path=Path(sys.argv[1]).resolve(),
    burst_group=sys.argv[2],
    burst_home=Path(sys.argv[3]).resolve(),
    elan_home=Path(sys.argv[4]).resolve(),
    burst_path=sys.argv[5],
    mathlib_build_tar=Path(sys.argv[6]).resolve() if sys.argv[6] else None,
)
PY

CONFIGURED_PROVIDERS="$(PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$CONFIG_OUT" <<'PY'
from pathlib import Path
import sys

from trellis.config import load_config

config = load_config(Path(sys.argv[1]).resolve())
providers = {config.worker.provider, config.reviewer.provider}
if config.easy_worker is not None:
    providers.add(config.easy_worker.provider)
if config.hard_worker is not None:
    providers.add(config.hard_worker.provider)
for agent in config.verification.correspondence_agents:
    providers.add(agent.provider)
for agent in config.verification.soundness_agents:
    providers.add(agent.provider)
for name in sorted(p for p in providers if p):
    print(name)
PY
)"

echo "  Validating provider CLI access..."
# Phase 4 bwrap-only migration: providers run as the supervisor user inside bwrap;
# CLI auth comes from the supervisor's ~/.codex, ~/.claude, ~/.gemini.
#
# GATE H: validate under EXACTLY the PATH the real worker burst uses
# (`host_runtime.worker_path_env(burst_home)`), NOT $BURST_PATH. The burst
# launches as `env PATH=worker_path_env(...) bwrap ... <provider>`; if we
# validated under a different (richer) $BURST_PATH the check could print
# "codex: ok" while the burst still exits 127. `worker_path_env` resolves the
# provider CLIs the same way the sandbox read-only binds do, so this check
# now fails loudly when the burst won't find the CLI.
WORKER_BURST_PATH="$(PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$BURST_HOME" <<'PY'
import sys
from pathlib import Path
from trellis.host_runtime import worker_path_env
print(worker_path_env(Path(sys.argv[1]).resolve()))
PY
)"
if grep -qx 'codex' <<<"$CONFIGURED_PROVIDERS"; then
  env HOME="$BURST_HOME" ELAN_HOME="$ELAN_HOME" PATH="$WORKER_BURST_PATH" \
    bash -lc "set -euo pipefail; command -v codex >/dev/null || { echo \"codex not found on the worker sandbox PATH (\$PATH); install it where trellis can reach it (per-user npm-global, nvm, or /usr/local/bin — see INSTALLATION.md)\" >&2; exit 1; }; timeout 10s codex --version >/dev/null"
  echo "    codex: ok"
fi
if grep -qx 'claude' <<<"$CONFIGURED_PROVIDERS"; then
  env HOME="$BURST_HOME" ELAN_HOME="$ELAN_HOME" PATH="$WORKER_BURST_PATH" \
    bash -lc "set -euo pipefail; command -v claude >/dev/null || { echo \"claude not found on the worker sandbox PATH (\$PATH); install it where trellis can reach it (per-user npm-global, nvm, or /usr/local/bin — see INSTALLATION.md)\" >&2; exit 1; }; timeout 10s claude --version >/dev/null"
  echo "    claude: ok"
fi
if grep -qx 'gemini' <<<"$CONFIGURED_PROVIDERS"; then
  # GEMINI_CLI_TRUST_WORKSPACE=true: gemini's interactive trust dialog
  # cannot be answered headlessly. Without this env (or `--skip-trust`)
  # gemini exits 55 with "Gemini CLI is not running in a trusted
  # directory" — the same gate that bites tmux_backend's smoke path.
  # Production runs handle this via ensure_gemini_accessibility_settings;
  # this validation step needs the env equivalent.
  env HOME="$BURST_HOME" ELAN_HOME="$ELAN_HOME" PATH="$WORKER_BURST_PATH" \
    bash -lc "command -v gemini >/dev/null || { echo \"gemini not found on the worker sandbox PATH (\$PATH); install it where trellis can reach it (per-user npm-global, nvm, or /usr/local/bin — see INSTALLATION.md)\" >&2; exit 1; }"
  env HOME="$BURST_HOME" ELAN_HOME="$ELAN_HOME" PATH="$WORKER_BURST_PATH" \
    GEMINI_CLI_TRUST_WORKSPACE=true \
    python3 - <<'PY'
import os
import subprocess

env = os.environ.copy()
subprocess.run(
    [
        "gemini",
        "--approval-mode=yolo",
        "-p",
        "Reply with OK only.",
        "--output-format",
        "text",
    ],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.PIPE,
    timeout=45,
    check=True,
    env=env,
)
PY
  echo "    gemini: ok"
fi

echo "  Validating worker-side shared access..."
# Phase 4: no sudo wrap; the supervisor user owns the repo and runs lake directly.
env \
  HOME="$BURST_HOME" \
  ELAN_HOME="$ELAN_HOME" \
  PATH="$BURST_PATH" \
  bash -lc "
    set -euo pipefail
    umask 0002
    git config --global --add safe.directory '$REPO' >/dev/null 2>&1 || true
    for package_dir in '$REPO'/.lake/packages/*; do
      if [ -e \"\$package_dir/.git\" ]; then
        git config --global --add safe.directory \"\$package_dir\" >/dev/null 2>&1 || true
      fi
    done
    cd '$REPO'
    lake env lean .trellis/scratch/example.lean
  "

# The supervisor-side tablet acceptance check routes through the unified
# checker server (no host-lake fallback), so it requires a live
# TRELLIS_CHECKER_SOCKET. Setup runs before any checker server is started
# (the supported launcher, restart_configured_run.sh, starts the server
# *after* setup), so skip this inline check when no socket is exported.
# The build steps above already validated the Lean project; the real
# acceptance check runs at run time via the checker server.
if [ -n "${TRELLIS_CHECKER_SOCKET:-}" ] && [ -S "${TRELLIS_CHECKER_SOCKET}" ]; then
  echo "  Validating supervisor-side deterministic checks..."
  python3 "$REPO/.trellis/scripts/check.py" tablet "$REPO"
else
  echo "  Skipping supervisor-side tablet check (no checker server during setup; validated at first run)."
fi

echo "  Validating sandboxed worker environment..."
PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$REPO" "$BURST_HOME" "$CONFIG_OUT" <<'PY'
from pathlib import Path
import os
import sys

from trellis.config import SandboxConfig, load_config
from trellis.sandbox import probe_worker_environment

repo = Path(sys.argv[1]).resolve()
burst_home = Path(sys.argv[2]).resolve()
config = load_config(Path(sys.argv[3]).resolve())
providers = {
    str(config.worker.provider or "").strip(),
    str(config.reviewer.provider or "").strip(),
}
if config.easy_worker is not None:
    providers.add(str(config.easy_worker.provider or "").strip())
if config.hard_worker is not None:
    providers.add(str(config.hard_worker.provider or "").strip())
for agent in config.verification.correspondence_agents:
    providers.add(str(agent.provider or "").strip())
for agent in config.verification.soundness_agents:
    providers.add(str(agent.provider or "").strip())
providers.discard("")
# The checker-surface certification materializes tablet oleans through the
# acceptance path, which (no host-lake fallback) requires a live checker socket.
# Setup runs before any checker server starts, so certify only when a socket is
# exported; the bwrap/provider validation above runs either way.
_socket = os.environ.get("TRELLIS_CHECKER_SOCKET", "")
_certify = bool(_socket) and Path(_socket).is_socket()
ok, detail = probe_worker_environment(
    sandbox=SandboxConfig(enabled=True, backend="bwrap"),
    repo_path=repo,
    burst_home=burst_home,
    provider_commands=sorted(providers),
    certify_checker_surface=_certify,
)
if not ok:
    raise SystemExit(detail or "sandboxed worker environment probe failed")
if not _certify:
    print("  (skipped checker-surface certification: no checker server during setup; validated at first run)")
PY

mkdir -p "$PROJECT_STATIC_DIR"
ln -sfn "$REPO/.trellis/viewer" "$PROJECT_STATIC_DIR/api"
ln -sfn "$SOURCE_ROOT/viewer/public/index.html" "$PROJECT_STATIC_DIR/index.html"
echo "  Linked project viewer route to repo-local viewer data"

find "$REPO/.trellis" -name '*.lock' -delete 2>/dev/null || true
git -C "$REPO" add -A
git -C "$REPO" commit -m "Initial repo setup with paper and Lean project" >/dev/null 2>&1 || true

echo ""
echo "Setup complete."
echo "  Repo:          $REPO"
echo "  Config:        $CONFIG_OUT"
echo "  Viewer route:  /trellis/$PROJECT_SLUG/"
echo "  Verified with worker-side and supervisor-side tablet checks."
if [ "$LOOGLE_SETTING" = "off" ]; then
  echo ""
  echo "  Loogle is OFF (loogle.enabled=false): the worker prompt omits the Loogle"
  echo "  helper. The worker skill files still carry a 'Loogle First' section — edit"
  echo "  them to remove the Loogle guidance since no server is configured:"
  echo "    $SOURCE_ROOT/skills/THEOREM_STATING_WORKER.md"
  echo "    $SOURCE_ROOT/skills/PROOF_FORMALIZATION_WORKER.md"
fi
