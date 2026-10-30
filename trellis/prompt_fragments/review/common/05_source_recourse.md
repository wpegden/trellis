## Source recourse

If the kernel-authored contract or these instructions appear to leave you with no legal decision that lets the run make forward progress, you may consult the trellis source as a recourse to ensure you (and the worker) understand the system constraints correctly.

The source is mounted read-only at `{{reviewer_source_snapshot_path}}` at git SHA `{{reviewer_source_sha}}`.

The current request uses acceptance logic `{{acceptance_logic_identity}}`. The mounted snapshot computes as `{{reviewer_source_acceptance_identity}}`; `reviewer_source_matches_acceptance={{reviewer_source_matches_acceptance}}`. Matching identities make this snapshot a reference for the current request's implementation. Otherwise, use it as historical context and rely on the kernel-authored contract for the current rules.

A prior worker rejection records its own `[acceptance_logic_identity]`. After an implementation upgrade, that historical identity can differ from the current request's identity. Keep the rejection associated with the rules that produced it; matching the current snapshot does not establish that the prior worker was judged by those same rules.

Useful starting points:

- `kernel/src/model.rs` — request-contract construction, allowed-decision computation
- `kernel/src/request_contracts.rs` — what the kernel actually emits to your prompt
- `kernel/src/runtime_cli_observations.rs` — worker-acceptance enforcement
- `trellis/runtime/bridge.py` — bridge-side normalization

Consult the source when process semantics or a worker's understanding of them blocks forward progress. Communicate any clarified process rule through the normal review fields. Record system defects in `system_feedback`.
