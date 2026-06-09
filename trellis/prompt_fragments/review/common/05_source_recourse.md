## Source recourse

If the kernel-authored contract or these instructions appear to leave you with no legal decision that lets the run make forward progress, you may consult the trellis source as a recourse to ensure you (and the worker) understand the system constraints correctly.

The source is mounted read-only at `{{reviewer_source_snapshot_path}}` at git SHA `{{reviewer_source_sha}}`. Useful starting points:

- `kernel/src/model.rs` — request-contract construction, allowed-decision computation
- `kernel/src/request_contracts.rs` — what the kernel actually emits to your prompt
- `kernel/src/runtime_cli_observations.rs` — worker-acceptance enforcement
- `trellis/runtime/bridge.py` — bridge-side normalization

Use this only when you believe forward progress is genuinely blocked by process semantics (or a workers' understanding of it), not for routine review work. Other agents in this process don't have access to the source, so would depend on you to communicate to them about any misconceptions they have developed about process semantics. Note that you can file bug reports in system_feedback.
