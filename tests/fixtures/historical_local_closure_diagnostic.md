# Historical Local-Closure Replay Diagnostic

Inspected sources:

- `${TRELLIS_ROOT:-/path/to/trellis}/math/designs-runtime/event_log.jsonl`
- `${TRELLIS_ROOT:-/path/to/trellis}/math/designs-runtime/protocol_state.json`
- `${TRELLIS_ROOT:-/path/to/trellis}/math/designs-runtime/checker-state/local-closure-*`
- `${TRELLIS_ROOT:-/path/to/trellis}/math/designs/.trellis-history/supervisor_state.json`
- `${TRELLIS_ROOT:-/path/to/trellis}/math/designs/.trellis/logs/check-ledger.jsonl`
- `${TRELLIS_ROOT:-/path/to/trellis}/math/offdiagonal-runtime/event_log.jsonl`
- `${TRELLIS_ROOT:-/path/to/trellis}/math/offdiagonal-runtime/protocol_state.json`
- `${TRELLIS_ROOT:-/path/to/trellis}/math/offdiagonal-runtime/checker-state/local-closure-*`
- `${TRELLIS_ROOT:-/path/to/trellis}/math/offdiagonal/.trellis-history/supervisor_state.json`
- `${TRELLIS_ROOT:-/path/to/trellis}/math/offdiagonal/.trellis/logs/check-ledger.jsonl`
- `supervisor2/checkpoint-*` git commits in both math repos

Reconstructable first-pass cases:

- `offdiagonal`: 119 first-pass `local_closure_results` cases; all have resolvable supervisor checkpoint commits.
- `designs`: 21 first-pass `local_closure_results` cases; all have resolvable supervisor checkpoint commits.

Known gap:

- `designs/.trellis/logs/check-ledger.jsonl` has 66 successful `local-closure-axioms` rows, but the ledger records only timing/subcommand metadata. It does not include node ids, event indices, request ids, snapshot ids, or checker payloads. Only 22 successful checker payloads for 21 unique nodes are present in `designs-runtime/event_log.jsonl`.
- The requested "around 100" current-run cases cannot be reconstructed from the available artifacts until a source is identified that maps those additional checker passes to node ids and first-pass Tablet snapshots.

Commands:

- Inventory and diagnostics: `python3 scripts/historical_local_closure_replay.py inventory --runs designs offdiagonal`
- Strict missing-artifact gate: `python3 scripts/historical_local_closure_replay.py inventory --runs designs offdiagonal --strict`
- Replay all reconstructable cases: `python3 scripts/historical_local_closure_replay.py replay --runs designs offdiagonal`
- Pytest wrapper: `pytest -q tests/test_historical_local_closure_replay.py`
- Opt-in replay smoke: `TRELLIS_RUN_HISTORICAL_LOCAL_CLOSURE_REPLAY=1 pytest -q tests/test_historical_local_closure_replay.py::test_historical_local_closure_replay_smoke`
