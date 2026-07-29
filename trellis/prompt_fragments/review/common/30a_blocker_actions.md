## Blocker action rule

For `continue` decisions, name only the obligations you are acting on in this transition. The kernel no longer requires a complete partition of every live blocker.

- `task_blocker_ids`: blockers assigned to the next worker. Soundness blockers are legal here only when `review_contract.blocker_actions.sound_repair_ready_nodes` contains the node.
- `reset_blocker_ids`: current Fail evidence you want to discard to Unknown so an ordinary verifier lane may run later. This is not a worker assignment.
- `request_sound_verifier_node_ids`: Sound node ids for a real Sound verifier run. This is the right action for stale Sound Pass or split Sound evidence before phase advancement. SKETCH nodes are never legal verifier targets. The kernel only dispatches Sound when every other verifier lane (paper, correspondence, substantiveness, deviation) is Pass across the board; while any other-lane blocker is open, the request is remembered but no Sound verifier fires.

Omitted blockers remain live. Use omission deliberately when a blocker is outside the transition you are routing now, when Sound reverification should be coalesced until known-fail work is gone, or when upstream statement/substantiveness/correspondence work must finish first.

For `need_input` and reset responses, leave all action lists empty unless the request contract explicitly says otherwise.
