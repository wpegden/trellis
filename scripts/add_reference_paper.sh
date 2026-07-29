#!/bin/bash
# Register an additional reference paper for a trellis run repo.
#
# Usage:
#   ./scripts/add_reference_paper.sh <repo> <tex_file> --id <slug> --source-id "<text>"
#
# Writes paper/refs/<id>.tex (UTF-8, transcoding when needed), appends the
# {id, tex_path, source_id} entry to workflow.reference_papers in
# <repo>/trellis.config.json, and commits BOTH files in the repo's git.
#
# For a run that already has kernel state, follow up during a stop window
# with the offline kernel action that syncs the config registry into state:
#   trellis_runtime_cli <<< '{"action":"add_reference_paper","root":"<runtime-root>","config_path":"<repo>/trellis.config.json"}'
# See REFERENCE_PAPERS.md for the full operator flow.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  cat <<'EOF'
Usage: ./scripts/add_reference_paper.sh <repo> <tex_file> --id <slug> --source-id "<text>"

  repo        Run repo root (contains trellis.config.json)
  tex_file    Reference paper .tex source (any common encoding; stored as UTF-8)
  --id        Registry slug (letters, digits, ., _, -); file lands at paper/refs/<slug>.tex
  --source-id Provenance text (citation / arXiv id / DOI)
EOF
}

REPO=""
TEX=""
REF_ID=""
SOURCE_ID=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --id)
      REF_ID="${2:?--id requires a value}"; shift 2 ;;
    --source-id)
      SOURCE_ID="${2:?--source-id requires a value}"; shift 2 ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      if [[ -z "$REPO" ]]; then REPO="$1"
      elif [[ -z "$TEX" ]]; then TEX="$1"
      else echo "ERROR: unexpected argument: $1" >&2; usage; exit 1
      fi
      shift ;;
  esac
done

if [[ -z "$REPO" || -z "$TEX" || -z "$REF_ID" || -z "$SOURCE_ID" ]]; then
  usage; exit 1
fi
if [[ ! "$REF_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
  echo "ERROR: --id must match [A-Za-z0-9][A-Za-z0-9._-]* (got: $REF_ID)" >&2
  exit 1
fi
CONFIG="$REPO/trellis.config.json"
if [[ ! -f "$TEX" ]]; then
  echo "ERROR: reference tex not found: $TEX" >&2
  exit 1
fi
if [[ ! -f "$CONFIG" ]]; then
  echo "ERROR: run config not found: $CONFIG" >&2
  exit 1
fi
REL_OUT="paper/refs/$REF_ID.tex"
OUT="$REPO/$REL_OUT"
if [[ -e "$OUT" ]]; then
  echo "ERROR: $OUT already exists; reference papers are immutable — use a new id" >&2
  exit 1
fi

# Duplicate-id config check BEFORE any file is written, so an abort
# leaves no orphan paper/refs/<id>.tex behind.
python3 - "$CONFIG" "$REF_ID" <<'PY'
import json
import sys

config_path, ref_id = sys.argv[1:3]
data = json.loads(open(config_path, encoding="utf-8").read())
if not isinstance(data, dict):
    raise SystemExit(f"{config_path} is not a JSON object")
entries = data.get("workflow", {}).get("reference_papers", [])
if not isinstance(entries, list):
    raise SystemExit("config.workflow.reference_papers is not an array")
for entry in entries:
    if isinstance(entry, dict) and entry.get("id") == ref_id:
        raise SystemExit(f"config already registers reference paper id `{ref_id}`")
PY

SCRATCH="$(mktemp -d "${TMPDIR:-${REPO}/.trellis/tmp}/add-reference.XXXXXX" 2>/dev/null || mktemp -d)"
cleanup() { rm -rf "$SCRATCH"; }
trap cleanup EXIT

# 1. Transcode to UTF-8 when needed (encoding-tolerant; iconv fallback).
UTF8_TEX="$SCRATCH/$REF_ID.utf8.tex"
if python3 - "$TEX" "$UTF8_TEX" <<'PY'
import sys
raw = open(sys.argv[1], "rb").read()
try:
    text = raw.decode("utf-8")
except UnicodeDecodeError:
    sys.exit(1)
open(sys.argv[2], "w", encoding="utf-8", newline="").write(text)
PY
then
  :
elif iconv -f WINDOWS-1252 -t UTF-8 "$TEX" > "$UTF8_TEX" 2>/dev/null; then
  echo "  note: transcoded $TEX from windows-1252 to UTF-8"
elif iconv -f LATIN1 -t UTF-8 "$TEX" > "$UTF8_TEX"; then
  echo "  note: transcoded $TEX from latin-1 to UTF-8"
else
  echo "ERROR: could not transcode $TEX to UTF-8" >&2
  exit 1
fi

# 2. Best-effort \newtheorem alias normalization — a convenience so grep-able
# env names match the primary paper's. Reference papers are read as prose
# grounding, so this step is NOT load-bearing: plain TeX (or a normalizer
# failure) passes the transcoded file through unchanged.
mkdir -p "$(dirname "$OUT")"
if python3 "$SCRIPT_DIR/normalize_paper_envs.py" "$UTF8_TEX" "$OUT" 2>/dev/null; then
  :
else
  cp "$UTF8_TEX" "$OUT"
fi
echo "  Wrote $REL_OUT"

# 3. Append the config entry (python; no jq dependency). Atomic rewrite:
# tmp + rename in the same directory (the kernel add-targets discipline),
# so a crash mid-write cannot clobber the operator's config. The
# duplicate-id check is repeated here as defence in depth.
python3 - "$CONFIG" "$REF_ID" "$REL_OUT" "$SOURCE_ID" <<'PY'
import json
import os
import sys

config_path, ref_id, rel_out, source_id = sys.argv[1:5]
data = json.loads(open(config_path, encoding="utf-8").read())
if not isinstance(data, dict):
    raise SystemExit(f"{config_path} is not a JSON object")
workflow = data.setdefault("workflow", {})
entries = workflow.setdefault("reference_papers", [])
if not isinstance(entries, list):
    raise SystemExit("config.workflow.reference_papers is not an array")
for entry in entries:
    if isinstance(entry, dict) and entry.get("id") == ref_id:
        raise SystemExit(f"config already registers reference paper id `{ref_id}`")
entries.append({"id": ref_id, "tex_path": rel_out, "source_id": source_id})
tmp_path = config_path + ".add-reference.tmp"
with open(tmp_path, "w", encoding="utf-8") as handle:
    handle.write(json.dumps(data, indent=2) + "\n")
os.replace(tmp_path, config_path)
PY
echo "  Appended workflow.reference_papers entry '$REF_ID' to $CONFIG"

# 4. Commit BOTH files with explicit pathspecs.
if git -C "$REPO" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  git -C "$REPO" add -- "$REL_OUT" "trellis.config.json"
  git -C "$REPO" commit -m "add_reference_paper: register $REF_ID ($SOURCE_ID)" -- "$REL_OUT" "trellis.config.json"
  echo "  Committed $REL_OUT + trellis.config.json"
else
  echo "  WARNING: $REPO is not a git worktree; commit $REL_OUT and trellis.config.json manually (a checkpoint reset --hard reverts uncommitted config edits)"
fi

echo "Done. For a run with existing kernel state, sync during a stop window:"
echo "  {\"action\":\"add_reference_paper\",\"root\":\"<runtime-root>\",\"config_path\":\"$CONFIG\"}"
