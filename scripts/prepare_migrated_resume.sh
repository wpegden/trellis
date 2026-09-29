#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  cat <<'EOF'
Usage:
  scripts/prepare_migrated_resume.sh <repo_path> <runtime_root> [--reconstruct-only] [--remote-resume]

This prepares an existing configured run for a migrated manual resume without
launching it:
  - backs up the current runtime/supervisor state
  - refuses to proceed if the repo has dirty semantic files outside runtime/operator scratch
  - refreshes the repo-local vendored runtime snapshot from current source
  - rebuilds the supervisor workspace from the repo snapshot
  - prewarms the rebuilt supervisor workspace with the same support/materialization path the resumed run will need
  - restores runtime state/checkpoint metadata from .trellis-history/supervisor_state.json
  - truncates the runtime event log back to the last accepted checkpoint
  - refreshes bridge latest_*.json artifacts from canonical history

It does not launch tmux or dispatch any agent work.
EOF
}

REPO_PATH="${1:-}"
RUNTIME_ROOT="${2:-}"
if [[ -z "$REPO_PATH" || -z "$RUNTIME_ROOT" ]]; then
  usage >&2
  exit 2
fi

REPO_PATH="$(python3 - "$REPO_PATH" <<'PY'
from pathlib import Path
import sys
print(Path(sys.argv[1]).resolve())
PY
)"
RUNTIME_ROOT="$(python3 - "$RUNTIME_ROOT" <<'PY'
from pathlib import Path
import sys
print(Path(sys.argv[1]).resolve())
PY
)"

python3 - "$ROOT_DIR" "$REPO_PATH" "$RUNTIME_ROOT" "${@:3}" <<'PY'
from __future__ import annotations

import json
import shutil
import subprocess
import sys
import time
from pathlib import Path

root_dir = Path(sys.argv[1]).resolve()
repo_path = Path(sys.argv[2]).resolve()
runtime_root = Path(sys.argv[3]).resolve()
options = set(sys.argv[4:])
if options - {"--reconstruct-only", "--remote-resume"}:
    raise SystemExit("unknown preparation option")
reconstruct_only = "--reconstruct-only" in options
remote_resume = "--remote-resume" in options
if remote_resume and not reconstruct_only:
    raise SystemExit("--remote-resume requires --reconstruct-only; warming is a separate stage")
if remote_resume:
    if (runtime_root / "trusted-artifact-rebind-publication").exists():
        raise SystemExit("refusing reconstruction over a staged or committed trusted migration")
    if (runtime_root / "sockets/checker.sock").exists():
        raise SystemExit("stop the destination checker before reconstructing its workspace")
    if (runtime_root / "resume-reconstruction.json").exists():
        print(json.dumps({"prepared": True, "reused": True}))
        raise SystemExit(0)

if str(root_dir) not in sys.path:
    sys.path.insert(0, str(root_dir))
from trellis.history_artifacts import decode_shared_state

if not repo_path.is_dir():
    raise SystemExit(f"repo_path does not exist: {repo_path}")
if not runtime_root.exists():
    raise SystemExit(f"runtime_root does not exist: {runtime_root}")

history_dir = repo_path / ".trellis-history"
supervisor_state_path = history_dir / "supervisor_state.json"
if not supervisor_state_path.is_file():
    raise SystemExit(f"missing canonical supervisor state snapshot: {supervisor_state_path}")

# Accepts both the historical plain snapshot and the trellis-shared-state/1
# container; detection is structural, and a plain document is returned as-is.
supervisor_state = decode_shared_state(json.loads(supervisor_state_path.read_text(encoding="utf-8")))
checkpoint_event_count = int(supervisor_state.get("event_count", 0) or 0)
if checkpoint_event_count <= 0:
    raise SystemExit(f"invalid checkpoint event_count in {supervisor_state_path}")
# Era convention (REWINDING.md §0.1): post-boundary checkpoints stamp
# event_count as the record count (aggregate == event_count lines);
# pre-boundary checkpoints stamped the last record index (monolith prefix ==
# event_count + 1 lines).
post_migration_line_count = checkpoint_event_count
pre_migration_prefix_line_count = checkpoint_event_count + 1

# Event-log handling depends on whether this checkpoint is at or after the
# per-cycle-segmentation boundary commit.
#
# POST-MIGRATION (the common case): the per-cycle event log lives in the
# tracked repo tree at `<repo>/.trellis-history/event-log/cycle-*.jsonl` and is
# already restored on disk by the `git reset --hard <tag>` that put the repo at
# this checkpoint. The kernel reads it straight from the repo — nothing to
# reconstruct here; we only verify the restored line count matches the
# checkpoint's event_count.
#
# PRE-MIGRATION fallback: old tags predate the per-cycle files, so the repo has
# no `event-log/` dir at this commit. Fall back to the archived monolith
# `<runtime>/event_log.jsonl.pre-segmentation`, truncate it to the checkpoint
# boundary, and split that prefix into per-cycle files (the kernel no longer
# reads a monolith). Any stale `event-log/` dir is cleared first.
event_log_dir = repo_path / ".trellis-history" / "event-log"
pre_seg_monolith = runtime_root / "event_log.jsonl.pre-segmentation"


def _cycle_files(directory):
    if not directory.is_dir():
        return []
    files = [
        p for p in directory.iterdir()
        if p.is_file() and p.name.startswith("cycle-") and p.name.endswith(".jsonl")
    ]
    files.sort(key=lambda p: p.name)
    return files


def _count_lines(files):
    total = 0
    for path in files:
        total += sum(1 for line in path.read_text(encoding="utf-8").splitlines() if line.strip())
    return total


restored_cycle_files = _cycle_files(event_log_dir)
is_post_migration = bool(restored_cycle_files)
if is_post_migration:
    restored = _count_lines(restored_cycle_files)
    if restored != post_migration_line_count:
        raise SystemExit(
            f"restored per-cycle event log has {restored} records, but checkpoint "
            f"event_count {checkpoint_event_count} needs exactly that many; "
            f"refusing to resume on a mismatched log"
        )
    event_lines = None  # not used for post-migration tags
else:
    if not pre_seg_monolith.is_file():
        raise SystemExit(
            f"checkpoint predates per-cycle segmentation (no event-log/ dir restored at this tag) "
            f"and the archived monolith {pre_seg_monolith} is missing; cannot reconstruct the event log"
        )
    event_lines = pre_seg_monolith.read_text(encoding="utf-8").splitlines()
    if len(event_lines) < pre_migration_prefix_line_count:
        raise SystemExit(
            f"pre-segmentation monolith only has {len(event_lines)} lines, but checkpoint "
            f"event_count {checkpoint_event_count} needs a {pre_migration_prefix_line_count}-line prefix"
        )

backup_root = runtime_root.parent / f"{runtime_root.name}-migrated-resume-backup-{time.strftime('%Y%m%d-%H%M%S')}"
backup_root.mkdir(parents=True, exist_ok=False)

backup_targets = {
    runtime_root: backup_root / "runtime",
    repo_path / ".trellis" / "supervisor": backup_root / "repo__.trellis__supervisor",
    repo_path / ".trellis-history" / "worker_state": backup_root / "repo__.trellis-history__worker_state",
    repo_path / ".trellis" / "checker": backup_root / "repo__.trellis__checker",
}
def _ignore_sockets(dirpath, names):
    # The prewarm step below needs the live checker server, whose bound
    # unix socket lives inside the runtime tree and cannot be copied.
    return [n for n in names if (Path(dirpath) / n).is_socket()]

for src, dst in backup_targets.items():
    if src.exists():
        if src.is_dir():
            shutil.copytree(src, dst, dirs_exist_ok=True, ignore=_ignore_sockets)
        else:
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dst)

git_status = subprocess.run(
    ["git", "status", "--short"],
    cwd=repo_path,
    text=True,
    capture_output=True,
    check=True,
).stdout.splitlines()

allowed_dirty_prefixes = (
    ".trellis/",
    ".trellis-history/worker_state/",
)
allowed_dirty_exact = {
    "THIS_RUN.md",
    ".trellis-creating",
}
semantic_dirty = []
for raw_line in git_status:
    entry = raw_line.strip()
    if not entry:
        continue
    path_text = entry[3:].strip() if len(entry) >= 4 else entry
    if " -> " in path_text:
        path_text = path_text.split(" -> ", 1)[1].strip()
    if path_text in allowed_dirty_exact or any(
        path_text.startswith(prefix) for prefix in allowed_dirty_prefixes
    ):
        continue
    semantic_dirty.append(entry)
if semantic_dirty:
    details = "\n".join(f"  {line}" for line in semantic_dirty)
    raise SystemExit(
        "refusing migrated resume prep on a semantically dirty repo worktree.\n"
        "restore the repo to the last accepted checkpoint first.\n"
        f"unexpected dirty entries:\n{details}"
    )

git_head = subprocess.run(
    ["git", "rev-parse", "HEAD"],
    cwd=repo_path,
    text=True,
    capture_output=True,
    check=True,
).stdout.strip()

sys.path.insert(0, str(root_dir))
from trellis.checking import write_scripts
from trellis.supervisor_workspace import (
    invalidate_content_stale_supervisor_build_artifacts,
    sync_supervisor_workspace,
)

write_scripts(repo_path, repo_path / ".trellis")

supervisor_repo = repo_path / ".trellis" / "supervisor" / "repo"
if supervisor_repo.exists():
    shutil.rmtree(supervisor_repo)
sync_supervisor_workspace(repo_path)

# Preserve the kernel-authoritative compiled oleans across the workspace rebuild
# so the prewarm REPLAYS instead of recompiling the whole tablet from empty.
# sync_supervisor_workspace copies sources + lake packages but deliberately not
# the Tablet build oleans; without this the prewarm's materialize rebuilds every
# node (a multi-hour storm on large runs). The live repo's `.lake` is the
# candidate build for the same just-synced sources. The freshness invalidation
# immediately below removes artifacts older than those sources before any bare
# Lean import can read them. Best-effort hardlink (same filesystem;
# atomic-rename rebuilds create new inodes, so the live tree is never
# perturbed).
import os as _os
_live_tablet_build = repo_path / ".lake" / "build" / "lib" / "lean" / "Tablet"
_ws_tablet_build = supervisor_repo / ".lake" / "build" / "lib" / "lean" / "Tablet"
if _live_tablet_build.is_dir():
    _ws_tablet_build.mkdir(parents=True, exist_ok=True)
    _mirror_suffixes = (".olean", ".olean.hash", ".olean.srcclosure", ".ilean", ".ilean.hash", ".trace")
    _mirrored = 0
    for _src in _live_tablet_build.iterdir():
        if not _src.is_file() or not any(_src.name.endswith(_s) for _s in _mirror_suffixes):
            continue
        _dst = _ws_tablet_build / _src.name
        try:
            if _dst.exists():
                _dst.unlink()
            _os.link(_src, _dst)
        except OSError:
            try:
                shutil.copy2(_src, _dst)
            except OSError:
                continue
        _mirrored += 1
    print(
        f"prep: mirrored {_mirrored} live Tablet build artifacts into workspace "
        "(prewarm will replay, not recompile)",
        file=sys.stderr,
    )

# A documented rewind resets sources but deliberately leaves the live repo's
# `.lake` in place.  The mirror above is only a prewarm optimization; discard
# any copied artifact whose provenance sidecar disagrees with the just-reset
# workspace sources before bare Lean imports can observe it.
_purged_stale_artifacts = invalidate_content_stale_supervisor_build_artifacts(
    supervisor_repo
)
if _purged_stale_artifacts:
    print(
        f"prep: invalidated {_purged_stale_artifacts} pre-rewind Tablet build "
        "artifacts older than the reset workspace sources",
        file=sys.stderr,
    )

checkpoint_data = supervisor_state.get("checkpoint")
state_data = supervisor_state.get("state")
metadata_data = supervisor_state.get("metadata")
if not isinstance(checkpoint_data, dict) or not isinstance(state_data, dict) or not isinstance(metadata_data, dict):
    raise SystemExit(f"malformed {supervisor_state_path}")

def normalize_label(value) -> str:
    return str(value or "").strip().replace("_", "").lower()

def backfill_legacy_proof_gate_defaults(state: dict) -> dict:
    """Fill gate booleans for the one pre-gate pending proof task shape.

    The legacy Easy proof route meant "no new open helpers and close active";
    legacy hard/local/restructure meant "new obligations allowed and active may
    remain open". New reviewer outputs must set these fields explicitly.
    """
    if normalize_label(state.get("phase")) != "proofformalization":
        return {"backfilled": False}
    task = state.get("pending_task")
    if not isinstance(task, dict):
        return {"backfilled": False}
    missing_allow = "allow_new_obligations" not in task
    missing_close = "must_close_active" not in task
    if not missing_allow and not missing_close:
        return {"backfilled": False}

    active = str(task.get("node") or state.get("active_node") or "").strip()
    difficulty_map = state.get("node_difficulty", {})
    active_difficulty = ""
    if isinstance(difficulty_map, dict) and active:
        active_difficulty = normalize_label(difficulty_map.get(active))
    mode = normalize_label(task.get("mode") or state.get("proof_edit_mode"))
    legacy_easy = active_difficulty == "easy" and mode in {"", "local"}
    allow_new_obligations = not legacy_easy
    must_close_active = legacy_easy

    if missing_allow:
        task["allow_new_obligations"] = allow_new_obligations
    if missing_close:
        task["must_close_active"] = must_close_active
    return {
        "backfilled": True,
        "active_node": active,
        "mode": task.get("mode"),
        "active_difficulty": active_difficulty or "unknown",
        "allow_new_obligations": task.get("allow_new_obligations"),
        "must_close_active": task.get("must_close_active"),
    }

def force_fresh_proof_boundary(state: dict, metadata: dict) -> dict:
    if normalize_label(state.get("phase")) != "proofformalization":
        return {"forced": False}
    removed = []
    kinds = metadata.get("native_history_kinds")
    if isinstance(kinds, list):
        filtered = []
        for kind in kinds:
            if kind in {"worker:proof_formalization", "review:proof_formalization"}:
                removed.append(kind)
            else:
                filtered.append(kind)
        metadata["native_history_kinds"] = filtered
    task = state.get("pending_task")
    task_context = None
    if isinstance(task, dict):
        task_context = task.get("next_worker_context_mode")
        task["next_worker_context_mode"] = "fresh"
    return {
        "forced": True,
        "removed_native_history_kinds": removed,
        "previous_pending_task_context": task_context,
    }

legacy_gate_backfill = {"backfilled": False} if remote_resume else backfill_legacy_proof_gate_defaults(state_data)
fresh_boundary = {"forced": False} if remote_resume else force_fresh_proof_boundary(state_data, metadata_data)
if remote_resume:
    from trellis.remote_resume import rebind_destination_paths
    config_path = repo_path / "trellis.config.json"
    config, metadata_data = rebind_destination_paths(json.loads(config_path.read_text()), metadata_data, repo_path, runtime_root)
    config_path.write_text(json.dumps(config, indent=2) + "\n")
    metadata_data["native_history_kinds"] = []
    sync_supervisor_workspace(repo_path)

check_script = supervisor_repo / ".trellis" / "scripts" / "check.py"
if not check_script.is_file():
    raise SystemExit(f"missing supervisor check script after sync: {check_script}")

def run_supervisor_check(*args: str, fatal: bool = True) -> bool:
    proc = subprocess.run(
        [sys.executable, str(check_script), *args],
        cwd=supervisor_repo,
        text=True,
        capture_output=True,
    )
    if proc.returncode == 0:
        return True
    details = []
    if proc.stdout.strip():
        details.append(f"stdout={proc.stdout.strip()!r}")
    if proc.stderr.strip():
        details.append(f"stderr={proc.stderr.strip()!r}")
    suffix = "" if not details else f"; {'; '.join(details)}"
    message = (
        f"supervisor workspace prewarm failed for {' '.join(args)!r} "
        f"with exit code {proc.returncode}{suffix}"
    )
    if fatal:
        raise SystemExit(message)
    # S3: prewarm / olean materialization is best-effort. The runtime state is
    # written before this runs (S5), so a failure here still leaves a LOADABLE
    # runtime; warn loudly and let the launch-time observe or a manual olean
    # rebuild recover, rather than aborting the whole resume.
    print(f"WARNING (non-fatal prewarm): {message}", file=sys.stderr)
    return False

committed = state_data.get("committed", {}) if isinstance(state_data, dict) else {}
present_nodes = committed.get("present_nodes", []) if isinstance(committed, dict) else []
requested_nodes = [str(name).strip() for name in present_nodes if str(name).strip()]
if "Preamble" not in requested_nodes:
    requested_nodes.insert(0, "Preamble")

# S5: write the runtime state files BEFORE the (best-effort) prewarm below so
# an aborted/failed prewarm still leaves a LOADABLE runtime at the checkpoint.
runtime_root.mkdir(parents=True, exist_ok=True)
(runtime_root / "protocol_state.json").write_text(
    json.dumps(state_data, indent=2) + "\n",
    encoding="utf-8",
)
(runtime_root / "checkpoint.json").write_text(
    json.dumps(checkpoint_data, indent=2) + "\n",
    encoding="utf-8",
)
# S1: recompute repo_path from the actual repo being prepped so a relocated
# clone resolves consistently on BOTH sides (the supervisor reads this, and
# the checker's _resolve_worker_repo_for_runtime now prefers it too).
if isinstance(metadata_data, dict):
    metadata_data["repo_path"] = str(repo_path)
(runtime_root / "runtime_metadata.json").write_text(
    json.dumps(metadata_data, indent=2) + "\n",
    encoding="utf-8",
)
# Event log: post-migration tags already have the correct per-cycle files
# restored in the repo (verified above) — leave them untouched. Pre-migration
# tags reconstruct the per-cycle files from the truncated archived monolith.
if not is_post_migration:
    if event_log_dir.exists():
        shutil.rmtree(event_log_dir)
    event_log_dir.mkdir(parents=True, exist_ok=True)
    by_cycle = {}
    for line in event_lines[:pre_migration_prefix_line_count]:
        if not line.strip():
            continue
        cycle = int(json.loads(line)["cycle"])
        by_cycle.setdefault(cycle, []).append(line)
    for cycle, lines in by_cycle.items():
        (event_log_dir / f"cycle-{cycle:06d}.jsonl").write_text(
            "\n".join(lines) + "\n",
            encoding="utf-8",
        )

# S3/S5: the runtime state is fully written above, so the prewarm + olean
# materialization run LAST and best-effort. A failure no longer aborts prep
# (the runtime is loadable); it is surfaced in the summary and on stderr.
prewarm_ok = None if reconstruct_only else run_supervisor_check(
    "prepare-compiled-support", str(supervisor_repo), fatal=False
)
materialize_args = ["materialize-tablet-oleans", str(supervisor_repo)]
for node in requested_nodes:
    materialize_args.extend(["--node", node])
materialize_ok = None if reconstruct_only else run_supervisor_check(*materialize_args, fatal=False)

bridge_dir = runtime_root / "bridge"
bridge_dir.mkdir(parents=True, exist_ok=True)
history_to_bridge = {
    "worker_handoff.json": "latest_worker.json",
    "paper_faithfulness_result.json": "latest_paper.json",
    "correspondence_result.json": "latest_corr.json",
    "soundness_result.json": "latest_sound.json",
    "reviewer_decision.json": "latest_review.json",
    "advance_gate_result.json": "latest_advance_gate.json",
}
for history_name, bridge_name in history_to_bridge.items():
    src = history_dir / history_name
    dst = bridge_dir / bridge_name
    if src.is_file():
        shutil.copy2(src, dst)
    else:
        dst.unlink(missing_ok=True)
    (bridge_dir / f"{bridge_name}.lock").unlink(missing_ok=True)

checker_dir = repo_path / ".trellis" / "checker"
if checker_dir.exists():
    shutil.rmtree(checker_dir)
worker_state_dir = history_dir / "worker_state"
if worker_state_dir.exists():
    shutil.rmtree(worker_state_dir)
repo_staging = repo_path / ".trellis" / "runtime" / "runtime" / "staging"
if repo_staging.exists():
    shutil.rmtree(repo_staging)

summary = {
    "prepared": True,
    "repo_path": str(repo_path),
    "runtime_root": str(runtime_root),
    "backup_root": str(backup_root),
    "repo_head": git_head,
    "repo_status": git_status,
    "checkpoint_event_count": checkpoint_event_count,
    "restored_event_line_count": (
        post_migration_line_count if is_post_migration else pre_migration_prefix_line_count
    ),
    "checkpoint_cycle": checkpoint_data.get("cycle"),
    "checkpoint_phase": checkpoint_data.get("phase"),
    "checkpoint_active_node": checkpoint_data.get("active_node"),
    "legacy_gate_backfill": legacy_gate_backfill,
    "fresh_boundary": fresh_boundary,
    "supervisor_repo": str(supervisor_repo),
    "prewarm_ok": prewarm_ok,
    "materialize_ok": materialize_ok,
}
if remote_resume:
    (runtime_root / "resume-reconstruction.json").write_text(json.dumps({"source_commit": git_head}) + "\n")
print(json.dumps(summary, indent=2))
PY
