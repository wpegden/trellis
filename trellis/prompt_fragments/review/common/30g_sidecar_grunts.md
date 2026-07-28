## Grunt workers (sidecar proof queue)

A pool of grunt workers runs beside the main loop, each trying to Lean-close one open proof node in an isolated repo copy. A grunt writes only the proof body below `-- BODY` — no imports, helper lemmas or definitions, statement edits, or other files — so it closes a node only when the proof fits in the body from existing imports and dependency lemmas; a node needing new imports, helpers, or restructuring is primary work. Every result passes the full kernel gate sequence before it can land.

A node is eligible when it is present, open, proof-kind, not a sketch placeholder, not the active node, held target, or a pending-task node, and its Substantiveness and Correspondence lanes Pass; the Sound lane is not consulted (Unknown and Fail are equally eligible). Closing a node retires its remaining soundness obligation — no further Sound verification.

Manage the queue in your response: `sidecar_queue_add` appends eligible nodes (worked front to back, one attempt each); `sidecar_queue_remove` drops them. An entry that has had its one attempt is dropped automatically once the result reaches the kernel. Re-adding the node mints a fresh attempt.

The primary workflow always wins: if it edits or closes a queued node first, the kernel discards the grunt result; removing a running node cancels its attempt; routing the worker to a queued node with `next_active` requires removing it in the same response; the kernel prunes a node that is deleted, closed, loses a statement-lane Pass, or has spent its one attempt, with the reason in the status table.

Decide all primary work — routing, tasks, blockers, resets — as though the grunt pool did not exist; grunt scheduling never bears on it. Separately, keep the pool on the most promising eligible nodes. The status table marks the tried eligible nodes it has room for, with how many attempts and how each ended, and counts the rest.

### Grunt status table

Alongside each node's attempt history, the table lists recent landed grunt closures (node, cycle, model) — the success side of the ledger.

{{sidecar_status_block}}

Eligible-now rows are capped at {{sidecar_status_prompt_line_limit}} lines; read `{{sidecar_candidates_path}}` and `{{sidecar_status_path}}` if the omitted part may matter.
