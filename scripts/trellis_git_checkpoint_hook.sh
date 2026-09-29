#!/usr/bin/env bash
set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PYTHONPATH="$ROOT_DIR${PYTHONPATH:+:$PYTHONPATH}"

# Capture stdin once so we can pipe the payload to the python hook AND
# inspect `is_clean` / `metadata.repo_path` afterwards. Use a runtime-local
# tmp dir; /tmp is a 32 GB partition that fills up on this host.
TMP_DIR="${TMPDIR:-/var/tmp}"
PAYLOAD_FILE=$(mktemp -p "$TMP_DIR" trellis-checkpoint-payload-XXXXXX.json)
trap 'rm -f "$PAYLOAD_FILE"' EXIT
cat > "$PAYLOAD_FILE"

# Run the existing git-checkpoint hook (commit + tag).
python3 -m trellis.runtime.git_checkpoint_hook < "$PAYLOAD_FILE"
HOOK_EXIT=$?

# Stop-at-next-clean integration. The supervisor's main loop checks for
# a `.trellis-stop-after-checkpoint` sentinel file in the repo path at
# the top of every step iteration and halts cleanly when present (state
# fully persisted at that point). To trigger a stop AT a specific clean
# checkpoint, place a `.stop_at_next_clean` sentinel under the runtime
# root: this hook will materialise the supervisor's stop sentinel on
# the next clean checkpoint and consume the runtime-side trigger so it
# fires only once.
#
# Backward-compatible: when the runtime-side sentinel is absent (the
# common case), this block is a no-op.
RUNTIME_ROOT=$(python3 - "$PAYLOAD_FILE" 2>/dev/null <<'PY' || true
import json, sys
try:
    payload = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)
print(str(payload.get("root", "")).strip())
PY
)
if [[ -n "$RUNTIME_ROOT" && -f "$RUNTIME_ROOT/.stop_at_next_clean" ]]; then
    REPO_IF_CLEAN=$(python3 - "$PAYLOAD_FILE" 2>/dev/null <<'PY' || true
import json, sys
try:
    payload = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)
if not payload.get("is_clean"):
    sys.exit(0)
metadata = payload.get("metadata") or {}
repo = str(metadata.get("repo_path", "") or "").strip()
if repo:
    print(repo)
PY
)
    if [[ -n "$REPO_IF_CLEAN" ]]; then
        touch "$REPO_IF_CLEAN/.trellis-stop-after-checkpoint"
        rm -f "$RUNTIME_ROOT/.stop_at_next_clean"
    fi
fi

# A/B config swap. Opt-in: enabled iff $TRELLIS_AB_TEMPLATES_DIR (or its
# $HOME/trellis-ab fallback) contains a template pair — either the generic
# `a.config.json` + `b.config.json`, or the legacy `codex.config.json` +
# `gemini.config.json`. Silent no-op otherwise. Fires only when the
# checkpoint leaves state at stage="Start" with no in-flight request — the
# kernel's precondition for emitting StartCycle next (runtime.rs:519-522,
# engine.rs:592). At that moment cycle N+1's first dispatch has not yet
# read trellis.config.json, so a copy here lands cleanly for the new
# cycle with no within-cycle split. Gated on $HOOK_EXIT==0 because a
# failed commit triggers kernel rollback and the same boundary will
# re-fire on retry.
AB_TEMPLATES_DIR="${TRELLIS_AB_TEMPLATES_DIR:-$HOME/trellis-ab}"
AB_PAIR=""
if [[ -f "$AB_TEMPLATES_DIR/a.config.json" && -f "$AB_TEMPLATES_DIR/b.config.json" ]]; then
    AB_PAIR="a b"
elif [[ -f "$AB_TEMPLATES_DIR/codex.config.json" && -f "$AB_TEMPLATES_DIR/gemini.config.json" ]]; then
    AB_PAIR="codex gemini"
fi
if [[ $HOOK_EXIT -eq 0 \
   && -n "$AB_TEMPLATES_DIR" \
   && -n "$AB_PAIR" ]]; then
    python3 - "$PAYLOAD_FILE" "$AB_TEMPLATES_DIR" $AB_PAIR \
        2>>"$AB_TEMPLATES_DIR/swap_errors.log" <<'PY' || true
import json, os, random, sys, time
payload_path, templates_dir = sys.argv[1], sys.argv[2]
pair = sys.argv[3:5] or ["codex", "gemini"]
log_path = os.path.join(templates_dir, "swap_log.jsonl")
try:
    payload = json.load(open(payload_path))
except Exception as exc:
    print(f"[ab] payload parse failed: {exc}", file=sys.stderr)
    sys.exit(0)
state = payload.get("state") or {}
if state.get("stage") != "Start" or state.get("in_flight_request") is not None:
    sys.exit(0)
repo_path = ((payload.get("metadata") or {}).get("repo_path") or "").strip()
if not repo_path:
    sys.exit(0)
cycle = (payload.get("checkpoint") or {}).get("cycle")
# Balanced blocks of 6, deterministic in the cycle number: cycles
# (6k..6k+5) form block k, assigned one of the C(6,3)=20 balanced
# arrangements by a hash of the block index. Block size chosen for exact
# randomization inference: ~0.72 bits/cycle of randomization entropy
# (vs 0.50 for pairs) for finer permutation p-values at small n, while
# within-block permutation still conditions out sub-block time drift and
# imbalance never exceeds 3. Deterministic hashing makes the assignment
# reproducible across checkpoint-retry re-fires and LastClean rewind
# replays (same cycle, same arm). Falls back to an iid coin only if the
# cycle number is unavailable.
if isinstance(cycle, int):
    import hashlib
    from itertools import combinations
    block, pos = divmod(cycle, 6)
    arrangements = list(combinations(range(6), 3))  # positions of pair[0]
    h = hashlib.sha256(f"trellis-ab:{templates_dir}:{block}".encode()).digest()
    idx = int.from_bytes(h[:4], "big") % len(arrangements)
    choice = pair[0] if pos in arrangements[idx] else pair[1]
else:
    choice = random.choice(pair)
src = os.path.join(templates_dir, f"{choice}.config.json")
dst = os.path.join(repo_path, "trellis.config.json")
if not os.path.isfile(src):
    sys.exit(0)
src_bytes = open(src, "rb").read()
try:
    dst_bytes = open(dst, "rb").read()
except FileNotFoundError:
    dst_bytes = b""
swapped = src_bytes != dst_bytes
if swapped:
    tmp = dst + ".ab-tmp"
    open(tmp, "wb").write(src_bytes)
    os.replace(tmp, dst)
with open(log_path, "a") as f:
    f.write(json.dumps({
        "ts": time.time(),
        "cycle": cycle,
        "choice": choice,
        "swapped": swapped,
        "repo_path": repo_path,
    }) + "\n")
PY
fi

exit $HOOK_EXIT
