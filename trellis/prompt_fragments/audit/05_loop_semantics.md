## Loop semantics

The audit may run for multiple bursts per round, up to `max_bursts_per_round` (default 5). Each burst:
- Receives the accumulated task list from prior bursts in this round (in `cleanup_audit_tasks`).
- Receives the scratchpad from the prior burst (in `cleanup_audit_scratchpad`).
- Emits a JSON response with `new_tasks`, `task_modifications`, `scratchpad_replace`, and `outcome`.

### Multi-burst rationale

The audit is allowed to take its time. If you've finished surveying the DAG by burst 1, return `outcome: audit_done` and stop. If you need more time to chase a particular wrapper candidate, return `outcome: need_to_continue` with a scratchpad note about what you're tracking, and continue in the next burst.

The kernel forces `audit_done` once `burst_count == max_bursts_per_round`. Use the budget; don't waste it.

### Revising your own proposals

`task_modifications` lets you mark a previously-proposed Pending task as `Dismissed { reason }`. This is the "I changed my mind on burst N+1 after closer inspection" channel. Rules:
- You may only revise **Pending** tasks. Terminal-status tasks (Completed / Failed / Dismissed) are immutable, regardless of which round originally proposed them.
- A round-2 audit may revise round-1 leftover Pending tasks the same way it revises its own round-2 proposals — Pending is Pending.
- The only legal transition is Pending → Dismissed. You cannot mark a task Completed or Failed (those are worker outcomes).

### Scratchpad

The `scratchpad_replace` field replaces the entire scratchpad — there's no append channel. Use it to track:
- Candidates you've considered and rejected (so you don't reconsider them next burst).
- Mathlib lemmas you've already searched for and didn't find.
- Open questions for the next burst.

The scratchpad is reset between rounds (the reviewer may request a re-audit; the new round starts with an empty scratchpad but the task list is preserved with terminal-status tasks intact).

### Rounds

After the audit produces tasks and the reviewer works through them, the reviewer may set `cleanup_request_reaudit = true` on a Done decision. If `cleanup_audit_round < max_rounds` (default 2), the kernel transitions back to a fresh audit round. You'll see:
- The task list with terminal-status entries from round 1 preserved (Completed/Failed/Dismissed).
- Any leftover Pending tasks from round 1 (you may revise them now).
- An empty scratchpad.
- `cleanup_audit_round = 2`.

Treat round 2 as "second-look after a worker has actually swung at the easy wins". This is the time to revisit tasks that depended on each other, or to propose tweaks based on what the round-1 workers learned.

### Previous burst rejection (if any)

If `latest_audit_rejection_reason` is a non-empty string on this request's audit contract payload, your **prior burst's output was rejected by the kernel's validator**. Read the rejection text and fix the malformed task(s) before continuing.

Typical rejection causes (from `legal_cleanup_task`):
- `target_node` not in `live.present_nodes`, or in the protected-statement set.
- A `Substitution.TabletWrapper` whose replacement isn't in `live.present_nodes`.
- A `LintFix` task with empty `warning_text`.
- A duplicate `(target_node, kind)` pair against the existing tasks list.
- A `task_modifications` entry whose `task_index` is out of range, or points to a non-Pending task (terminal-status tasks are immutable across rounds).

The kernel allows **one retry** per burst slot. A second consecutive validation failure on the same burst forces `outcome = AuditDone` with whatever has been validly accumulated in prior bursts. Do not resubmit unchanged — the same rejection will fire again and burn your retry. Fix every cited issue, then resubmit.

When `latest_audit_rejection_reason` is empty (the typical case on a fresh burst), there is nothing to act on — proceed normally.
