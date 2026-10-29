#!/bin/bash
# Prepare a revision-mode working repo from an existing tablet repo and a newer
# paper version (revision_plan.md §4, §16 step 6).
#
# This script does NOT run the revision planner. It only:
#   1. copies the base tablet repo working tree to --out-repo (fails if it
#      already exists; no overwrite in v1);
#   2. copies both paper sources to paper/revision/{old,new}.tex;
#   3. points workflow.paper_tex_path at paper/revision/new.tex and records the
#      workflow.revision.{old_paper_tex_path,old_source_id,new_source_id} block;
#   4. preserves the existing Tablet/ directory verbatim;
#   5. calls the `import_revision_project` runtime CLI action to create the
#      initial RevisionStating state at --runtime-root (a sibling
#      <out-repo>-runtime by default), which seeds Stage::StuckMathAudit and
#      issues the first revision-planning audit request.
#
# After this, point the normal launcher (scripts/restart_configured_run.sh
# --no-reset <out-repo>/trellis.config.json <runtime-root>) at the prepared
# repo and runtime root; --no-reset preserves the seeded RevisionStating state
# (a bare restart would wipe it) and the supervisor loop dispatches the seeded
# StuckMathAudit.
#
# Usage:
#   ./scripts/setup_revision_repo.sh \
#     --base-repo <existing-tablet-repo> \
#     --old-paper <old-main-tex> \
#     --new-paper <new-main-tex> \
#     --out-repo <new-working-repo> \
#     [--project-slug <slug>] \
#     [--target-map <target-map-json>] \
#     [--old-source-id <id>] \
#     [--new-source-id <id>] \
#     [--runtime-root <runtime-root>] \
#     [--copy-mode copy|clone]

set -euo pipefail
umask 0002

usage() {
  cat <<'EOF'
Usage: ./scripts/setup_revision_repo.sh \
  --base-repo <existing-tablet-repo> \
  --old-paper <old-main-tex> \
  --new-paper <new-main-tex> \
  --out-repo <new-working-repo> \
  [--project-slug <slug>] \
  [--target-map <target-map-json>] \
  [--old-source-id <id>] \
  [--new-source-id <id>] \
  [--runtime-root <runtime-root>] \
  [--copy-mode copy|clone]

  --base-repo     Existing, already-formalized tablet repo to revise. Must hold
                  a prior full state at .trellis-history/supervisor_state.json.
  --old-paper     The paper source the base tablet was formalized against.
  --new-paper     The newer paper source to revise the tablet against.
  --out-repo      Where to create the new working repo. Must NOT already exist.
  --project-slug  Optional viewer/session slug (defaults to basename(out-repo)).
  --target-map    Optional configured-target -> TeX-label override JSON, passed
                  to import_revision_project (decision 1).
  --old-source-id Optional provenance string for the old paper (e.g. arXiv:..v1).
  --new-source-id Optional provenance string for the new paper (e.g. arXiv:..v3).
  --runtime-root  Optional runtime state root (defaults to <out-repo>-runtime).
                  This is where protocol_state.json is created; pass the same
                  path to restart_configured_run.sh when launching.
  --copy-mode     copy (default) copies the base repo working tree; clone uses
                  `git clone` of the base repo working tree.
EOF
}

BASE_REPO=""
OLD_PAPER=""
NEW_PAPER=""
OUT_REPO=""
PROJECT_SLUG=""
TARGET_MAP=""
OLD_SOURCE_ID=""
NEW_SOURCE_ID=""
RUNTIME_ROOT=""
COPY_MODE="copy"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base-repo)     [[ $# -ge 2 ]] || { echo "ERROR: --base-repo requires an argument" >&2; exit 1; }; BASE_REPO="$2"; shift 2 ;;
    --old-paper)     [[ $# -ge 2 ]] || { echo "ERROR: --old-paper requires an argument" >&2; exit 1; }; OLD_PAPER="$2"; shift 2 ;;
    --new-paper)     [[ $# -ge 2 ]] || { echo "ERROR: --new-paper requires an argument" >&2; exit 1; }; NEW_PAPER="$2"; shift 2 ;;
    --out-repo)      [[ $# -ge 2 ]] || { echo "ERROR: --out-repo requires an argument" >&2; exit 1; }; OUT_REPO="$2"; shift 2 ;;
    --project-slug)  [[ $# -ge 2 ]] || { echo "ERROR: --project-slug requires an argument" >&2; exit 1; }; PROJECT_SLUG="$2"; shift 2 ;;
    --target-map)    [[ $# -ge 2 ]] || { echo "ERROR: --target-map requires an argument" >&2; exit 1; }; TARGET_MAP="$2"; shift 2 ;;
    --old-source-id) [[ $# -ge 2 ]] || { echo "ERROR: --old-source-id requires an argument" >&2; exit 1; }; OLD_SOURCE_ID="$2"; shift 2 ;;
    --new-source-id) [[ $# -ge 2 ]] || { echo "ERROR: --new-source-id requires an argument" >&2; exit 1; }; NEW_SOURCE_ID="$2"; shift 2 ;;
    --runtime-root)  [[ $# -ge 2 ]] || { echo "ERROR: --runtime-root requires an argument" >&2; exit 1; }; RUNTIME_ROOT="$2"; shift 2 ;;
    --copy-mode)
      [[ $# -ge 2 ]] || { echo "ERROR: --copy-mode requires an argument: copy or clone" >&2; exit 1; }
      case "$2" in copy|clone) COPY_MODE="$2" ;; *) echo "ERROR: --copy-mode must be 'copy' or 'clone', got: $2" >&2; exit 1 ;; esac
      shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "ERROR: Unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
done

if [[ -z "$BASE_REPO" || -z "$OLD_PAPER" || -z "$NEW_PAPER" || -z "$OUT_REPO" ]]; then
  echo "ERROR: --base-repo, --old-paper, --new-paper, and --out-repo are all required." >&2
  usage >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

resolve() {
  python3 - "$1" <<'PY'
from pathlib import Path
import sys
print(Path(sys.argv[1]).resolve())
PY
}

BASE_REPO="$(resolve "$BASE_REPO")"
OLD_PAPER="$(resolve "$OLD_PAPER")"
NEW_PAPER="$(resolve "$NEW_PAPER")"
OUT_REPO="$(resolve "$OUT_REPO")"
if [[ -n "$TARGET_MAP" ]]; then TARGET_MAP="$(resolve "$TARGET_MAP")"; fi

DEFAULT_SLUG="$(basename "$OUT_REPO" | sed -E 's/_tablets?$//')"
PROJECT_SLUG="${PROJECT_SLUG:-$DEFAULT_SLUG}"
if [[ -z "$RUNTIME_ROOT" ]]; then
  RUNTIME_ROOT="${OUT_REPO}-runtime"
else
  RUNTIME_ROOT="$(resolve "$RUNTIME_ROOT")"
fi

if [[ ! -d "$BASE_REPO" ]]; then
  echo "ERROR: --base-repo is not a directory: $BASE_REPO" >&2
  exit 1
fi
if [[ ! -d "$BASE_REPO/Tablet" ]]; then
  echo "ERROR: base repo has no Tablet/ directory: $BASE_REPO/Tablet" >&2
  exit 1
fi
BASE_STATE="$BASE_REPO/.trellis-history/supervisor_state.json"
if [[ ! -f "$BASE_STATE" ]]; then
  echo "ERROR: base repo has no prior full state at $BASE_STATE" >&2
  echo "       (revision import v1 requires the full_state path; run the base" >&2
  echo "        formalization to completion so it persists supervisor_state.json)" >&2
  exit 1
fi
if [[ ! -f "$OLD_PAPER" ]]; then echo "ERROR: --old-paper not found: $OLD_PAPER" >&2; exit 1; fi
if [[ ! -f "$NEW_PAPER" ]]; then echo "ERROR: --new-paper not found: $NEW_PAPER" >&2; exit 1; fi
if [[ ! -f "$BASE_REPO/trellis.config.json" ]]; then
  echo "ERROR: base repo has no trellis.config.json: $BASE_REPO/trellis.config.json" >&2
  exit 1
fi

# v1: never overwrite an existing out-repo.
if [[ -e "$OUT_REPO" ]]; then
  echo "ERROR: --out-repo already exists: $OUT_REPO" >&2
  echo "       (no overwrite in v1; choose a fresh path or remove it first)" >&2
  exit 1
fi
if [[ -e "$RUNTIME_ROOT" ]]; then
  echo "ERROR: --runtime-root already exists: $RUNTIME_ROOT" >&2
  echo "       (no overwrite in v1; choose a fresh path or remove it first)" >&2
  exit 1
fi

echo "Preparing revision repo:"
echo "  Base repo:    $BASE_REPO"
echo "  Out repo:     $OUT_REPO"
echo "  Old paper:    $OLD_PAPER"
echo "  New paper:    $NEW_PAPER"
echo "  Project slug: $PROJECT_SLUG"
echo "  Runtime root: $RUNTIME_ROOT"
echo "  Copy mode:    $COPY_MODE"

# 1. Copy the base repo working tree to --out-repo.
mkdir -p "$(dirname "$OUT_REPO")"
case "$COPY_MODE" in
  copy)
    cp -a "$BASE_REPO" "$OUT_REPO"
    ;;
  clone)
    git clone --quiet "$BASE_REPO" "$OUT_REPO"
    # `git clone` reproduces only tracked content; bring the working-tree-only
    # support dirs (papers under revision/, Tablet support) across verbatim too.
    cp -a "$BASE_REPO/Tablet/." "$OUT_REPO/Tablet/"
    ;;
esac
echo "  Copied base repo working tree to $OUT_REPO"

# Drop any stale prior runtime state copied along with the working tree; the
# new run gets a fresh runtime root created by import_revision_project below.
rm -rf "$OUT_REPO/.trellis/runtime"
# Clear the base run's accumulated VIEWER + chat history: cp -a carries the whole
# base run's chats, per-cycle event-log, and progress caches. The viewer then (a)
# renders chats via a python subprocess that ENOBUFS-overflows on the volume
# (HTTP 500 -> blank chat panel) and (b) shows the base run's 140 cycles in the
# cycle dropdown instead of the revision run's. The revision run starts fresh.
rm -rf "$OUT_REPO/.trellis/chats/live" "$OUT_REPO/.trellis/chats/cycle-"*
rm -rf "$OUT_REPO/.trellis-history/event-log"
rm -f "$OUT_REPO/.trellis/viewer/progress-cache-v8.json" \
      "$OUT_REPO/.trellis/viewer/progress-series-cache-v8.json"
# NOTE: do NOT remove .trellis/supervisor — it carries the base tablet's prebuilt
# .lake oleans / lean-semantic payloads, which the startup integrity guard needs
# to recompute correspondence/soundness fingerprints from disk (without them the
# guard sees empty disk fingerprints and halts with "state diverges from disk").
# Its config copy IS stale (pins the base providers), and the bridge resolves
# agent bindings from it — so after editing role engines in the out-repo config,
# sync that config into .trellis/supervisor/repo/trellis.config.json before
# launching (see the closing guidance).

# 2. Copy both paper sources into paper/revision/.
mkdir -p "$OUT_REPO/paper/revision"
cp "$OLD_PAPER" "$OUT_REPO/paper/revision/old.tex"
cp "$NEW_PAPER" "$OUT_REPO/paper/revision/new.tex"
echo "  Copied papers to paper/revision/{old,new}.tex"

# 3. Point workflow.paper_tex_path at the new paper and record the revision
#    block. The §3 invariant (paper_tex_path == new.tex) is asserted by
#    import_revision_project; we set it here so that holds.
CONFIG_OUT="$OUT_REPO/trellis.config.json"
python3 - "$CONFIG_OUT" "$OUT_REPO" "$OLD_SOURCE_ID" "$NEW_SOURCE_ID" <<'PY'
import json
import sys
from pathlib import Path

config_path = Path(sys.argv[1]).resolve()
out_repo = Path(sys.argv[2]).resolve()
old_source_id = sys.argv[3]
new_source_id = sys.argv[4]

data = json.loads(config_path.read_text(encoding="utf-8"))
if not isinstance(data, dict):
    raise SystemExit("trellis.config.json must be a JSON object")

# Point repo_path at the new working repo so the runtime resolves against it.
data["repo_path"] = str(out_repo)

# Repoint absolute, base-repo-baked locations at the out-repo. Without this the
# revision run writes burst chat logs into the BASE repo's chat dir (polluting
# the original tablet) and the viewer/sessions mis-point.
state_dir = str(data.get("state_dir", ".trellis") or ".trellis")
chat = data.setdefault("chat", {})
chat["root_dir"] = str(out_repo / state_dir / "chats")
chat["repo_name"] = out_repo.name
tmux_cfg = data.get("tmux")
if isinstance(tmux_cfg, dict):
    tmux_cfg["session_name"] = out_repo.name

workflow = data.setdefault("workflow", {})
workflow["paper_tex_path"] = "paper/revision/new.tex"
revision = workflow.setdefault("revision", {})
revision["old_paper_tex_path"] = "paper/revision/old.tex"
if old_source_id:
    revision["old_source_id"] = old_source_id
if new_source_id:
    revision["new_source_id"] = new_source_id

config_path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
PY
echo "  Set workflow.paper_tex_path -> paper/revision/new.tex (+ revision block)"

# 4. Tablet/ is preserved verbatim by the copy above (sanity check).
if [[ ! -d "$OUT_REPO/Tablet" ]]; then
  echo "ERROR: Tablet/ missing in out-repo after copy: $OUT_REPO/Tablet" >&2
  exit 1
fi
echo "  Preserved Tablet/ ($(find "$OUT_REPO/Tablet" -maxdepth 1 -name '*.lean' | wc -l | tr -d ' ') node .lean files)"

# 4b. Materialize the runtime support scripts into the out-repo's
#     .trellis/scripts (the equivalent of what `trellis.sh init` does via
#     materialize_runtime_support_from_config -> write_scripts). The out-repo's
#     .trellis/scripts was copied verbatim from the base repo and may be stale;
#     refresh it against THIS config. import_revision_project (step 5) invokes
#     the repo's check.py sync_tablet_support_op, so these must exist first.
PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - "$CONFIG_OUT" <<'PY'
from pathlib import Path
import sys

from trellis.checking import write_scripts
from trellis.config import load_config

config = load_config(Path(sys.argv[1]).resolve())
write_scripts(config.repo_path, config.state_dir)
PY
echo "  Materialized runtime support scripts into $OUT_REPO/.trellis/scripts"

# 5. Create the initial RevisionStating state via import_revision_project.
mkdir -p "$RUNTIME_ROOT"
PYTHONPATH="$SOURCE_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 - \
    "$RUNTIME_ROOT" "$CONFIG_OUT" "$BASE_STATE" "$OUT_REPO/paper/revision/old.tex" \
    "$OUT_REPO/paper/revision/new.tex" "$OLD_SOURCE_ID" "$NEW_SOURCE_ID" "$TARGET_MAP" <<'PY'
import json
import sys
from pathlib import Path

from trellis.runtime.kernel_cli import run_kernel_cli

runtime_root = Path(sys.argv[1]).resolve()
config_path = Path(sys.argv[2]).resolve()
base_state = Path(sys.argv[3]).resolve()
old_paper = Path(sys.argv[4]).resolve()
new_paper = Path(sys.argv[5]).resolve()
old_source_id = sys.argv[6]
new_source_id = sys.argv[7]
target_map_path = sys.argv[8]

payload = {
    "action": "import_revision_project",
    "root": str(runtime_root),
    "config_path": str(config_path),
    "full_state_path": str(base_state),
    "old_paper_tex_path": str(old_paper),
    "new_paper_tex_path": str(new_paper),
    "old_source_id": old_source_id,
    "new_source_id": new_source_id,
}
if target_map_path:
    payload["target_map"] = json.loads(Path(target_map_path).read_text(encoding="utf-8"))

result = run_kernel_cli(payload)
if result.get("status") != "import_revision_project_ok":
    raise SystemExit(f"import_revision_project failed: {json.dumps(result, indent=2)}")

summary = result.get("summary", {})
state = result.get("state", {})
print("  import_revision_project ok:")
print(f"    phase/stage:   {state.get('phase', '?')} / {state.get('stage', '?')}")
print(
    "    targets:       "
    f"unchanged={summary.get('unchanged_targets', '?')} "
    f"changed={summary.get('changed_targets', '?')} "
    f"added={summary.get('added_targets', '?')} "
    f"removed={summary.get('removed_targets', '?')}"
)
print(
    "    nodes:         "
    f"present={summary.get('present_nodes', '?')} "
    f"frozen={summary.get('frozen_nodes', '?')} "
    f"editable={summary.get('editable_nodes', '?')}"
)
for note in summary.get("notes", []):
    print(f"    note:          {note}")
PY

echo ""
echo "Revision repo prepared."
echo "  Repo:          $OUT_REPO"
echo "  Config:        $CONFIG_OUT"
echo "  Runtime root:  $RUNTIME_ROOT"
echo ""
echo "Launch with:"
echo "  scripts/restart_configured_run.sh --no-reset $CONFIG_OUT $RUNTIME_ROOT"
echo "The first dispatched request is the revision-planning StuckMathAudit."
echo ""
echo "Agent engines: the bridge resolves bindings from the SUPERVISOR-REPO config"
echo "copy, which is the base tablet's (stale). After editing role engines in"
echo "$CONFIG_OUT, sync them before launching:"
echo "  cp $CONFIG_OUT $OUT_REPO/.trellis/supervisor/repo/trellis.config.json"
echo ""
echo "Soundness fingerprint mode: the run recomputes soundness fingerprints from"
echo "disk and a startup integrity guard rejects the seed if they diverge from the"
echo "inherited (base-tablet) fingerprints. The launcher defaults to v2_strict, but"
echo "a legacy/partial-\\noderef base tablet was fingerprinted under v2_permissive."
echo "If launch halts with 'kernel state diverges from disk', relaunch with the"
echo "base tablet's mode, e.g.:"
echo "  TRELLIS_SOUNDNESS_FINGERPRINT_MODE=v2_permissive \\"
echo "    scripts/restart_configured_run.sh --no-reset $CONFIG_OUT $RUNTIME_ROOT"
