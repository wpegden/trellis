## Blocker action rule

For `continue` decisions, name only the obligations you are acting on in this transition. The kernel no longer requires a complete partition of every live blocker.

- `task_blocker_ids`: blockers assigned to the next worker. Soundness blockers are legal here only when `review_contract.blocker_actions.sound_repair_ready_nodes` contains the node.
- `reset_blocker_ids`: current Fail evidence you want to discard to Unknown so an ordinary verifier lane may run later. This is not a worker assignment.
- `request_sound_verifier_node_ids`, when offered: queue locally eligible nodes for a real Sound verifier run. SKETCH nodes are never legal targets. Acceptance means the request can be queued; it does not promise immediate dispatch. Ordinary Sound dispatch waits for the correspondence, substantiveness, and deviation statement guards to be Pass. Global paper-target coverage is a separate gate.

Use `review_contract.blocker_actions.sound_dispatch_readiness`, when present, to distinguish queueable nodes from `dispatchable_requested_nodes` and inspect `surface_blockers`. Its `next_verifier_without_worker` describes the next available verifier with no worker assignment; `progress_reset_blockers` lists legal resets that open a verifier lane, not recommendations to discard evidence. Deferred surface blockers explain scheduling and are not additional legal blocker actions.

A verification-only Continue has no `next_active`, worker blockers, worker scope authorizations, or reset/revert choice, and requests Sound. When `verifier_only_continuation_enforced` is true, this shape must have a runnable Sound or prerequisite verifier, including one opened by a legal `reset_blocker_ids` action. Otherwise the checker rejects it and the reviewer must choose a legal repair or audit transition. Naming `next_active` or assigning worker blockers expresses worker work while Sound remains queued. For older contracts without this guard, a blocked empty-active Continue can still route a worker; do not use it to request a verification-only pause.

If an audit plan requires Sound while preserving the failures that block it, reconsider that ordering through an admissible `audit_request` with `reason_kind: "suspect_report"`. Audit tasks are advisory. Retain valid failure evidence; resetting it merely to force scheduling does not resolve the mathematical obstruction.

Omitted blockers remain live. Use omission deliberately when a blocker is outside the transition you are routing now, when Sound reverification should be coalesced until known-fail work is gone, or when upstream statement/substantiveness/correspondence work must finish first.

For `need_input` and reset responses, leave all action lists empty unless the request contract explicitly says otherwise.
