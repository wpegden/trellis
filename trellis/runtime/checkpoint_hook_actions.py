"""Optional checkpoint launcher actions, using the hook's parsed payload.

Keep these outside commit_checkpoint: direct callers do not opt into the
launcher's stop sentinel or A/B configuration switching behavior.
"""
from __future__ import annotations

import hashlib
from itertools import combinations
import json
import os
from pathlib import Path
import random
import sys
import time


def _stop_at_clean(payload):
    root = str(payload.get("root", "")).strip()
    if not root or not payload.get("is_clean"):
        return
    trigger = Path(root) / ".stop_at_next_clean"
    if not trigger.is_file():
        return
    repo = str((payload.get("metadata") or {}).get("repo_path", "") or "").strip()
    if repo:
        (Path(repo) / ".trellis-stop-after-checkpoint").touch()
        trigger.unlink()


def _swap_config(payload, templates, pair):
    state = payload.get("state") or {}
    if state.get("stage") != "Start" or state.get("in_flight_request") is not None:
        return
    repo = str((payload.get("metadata") or {}).get("repo_path", "") or "").strip()
    if not repo:
        return
    cycle = (payload.get("checkpoint") or {}).get("cycle")
    # Preserve the original deterministic, balanced blocks of six. Retry and
    # LastClean rewind must choose the same arm for the same cycle.
    if isinstance(cycle, int):
        block, pos = divmod(cycle, 6)
        arrangements = list(combinations(range(6), 3))
        digest = hashlib.sha256(f"trellis-ab:{templates}:{block}".encode()).digest()
        choice = pair[0] if pos in arrangements[int.from_bytes(digest[:4], "big") % len(arrangements)] else pair[1]
    else:
        choice = random.choice(pair)
    source = Path(templates) / f"{choice}.config.json"
    if not source.is_file():
        return
    destination = Path(repo) / "trellis.config.json"
    source_bytes = source.read_bytes()
    try:
        previous = destination.read_bytes()
    except FileNotFoundError:
        previous = b""
    swapped = source_bytes != previous
    if swapped:
        temporary = Path(str(destination) + ".ab-tmp")
        temporary.write_bytes(source_bytes)
        os.replace(temporary, destination)
    with (Path(templates) / "swap_log.jsonl").open("a") as stream:
        stream.write(json.dumps(dict(ts=time.time(), cycle=cycle, choice=choice,
                                     swapped=swapped, repo_path=repo)) + "\n")


def run(payload, status):
    # Historically the stop sentinel was independent of hook success. Keep
    # that behavior; configuration switching is success-only because a failed
    # checkpoint rolls the runtime back and re-fires the boundary on retry.
    try:
        _stop_at_clean(payload)
    except Exception as exc:
        print(f"checkpoint stop action failed: {exc}", file=sys.stderr)
    if status != 0:
        return
    templates = os.environ.get("TRELLIS_AB_TEMPLATES_DIR") or str(Path.home() / "trellis-ab")
    pair = next((pair for pair in (("a", "b"), ("codex", "gemini"))
                 if all((Path(templates) / f"{name}.config.json").is_file() for name in pair)), None)
    if pair is None:
        return
    try:
        _swap_config(payload, templates, pair)
    except Exception as exc:
        # Preserve the best-effort optional action and its diagnostic file.
        try:
            with (Path(templates) / "swap_errors.log").open("a") as stream:
                stream.write(f"[ab] config swap failed: {exc}\n")
        except OSError:
            pass


if __name__ == "__main__":
    from trellis.runtime.git_checkpoint_hook import main
    raise SystemExit(main(after_checkpoint=run))
