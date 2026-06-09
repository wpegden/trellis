## Why you were called

`audit_latch_json.trigger` names the kernel signal that fired:

- `sound-stagnation-window: ... (theorem-stating | proof-formalization)` — some node is not-sound now, some node was not-sound at least N cycles ago, and no node present at both endpoints has gone from not-sound to sound.
- `cycles_since_clean >= N ...` — open blockers (any kind) have persisted for N+ checkpoints.
- `cycles_since_shallow_coarse_closed_count_increase >= N ...` — coarse-DAG shallow closure has not advanced for N+ checkpoints.
- `reviewer requested NeedInput: ...` — reviewer explicitly escalated; the detail follows the colon.
- `forced after reset/rewind` — a `LastClean` or cone-clean rewind just landed; fresh audit on the rewound state.
- `reviewer requested global_repair extension: ...` — see the GlobalRepairAuditor role fragment.

If the prefix does not match, treat it as the `cycles_since_clean` variant and check `request_summary_json`.
