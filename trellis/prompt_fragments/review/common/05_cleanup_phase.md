## Cleanup phase (v2 — task-driven)

Formalization is done. The audit has produced a list of cleanup tasks (`cleanup_audit_tasks` in the request). Each task is one of:
- **Substitution** — delete a wrapper node and inline its replacement (a mathlib lemma or another tablet node) at every importer.
- **LintFix** — single-node hygiene edit driven by a lake-build warning.

Each task has a status:
- **Pending** — eligible for dispatch or dismissal.
- **Completed** — a worker burst applied this task successfully.
- **Failed** — a worker burst attempted this task and failed (single attempt; not retried).
- **Dismissed** — you or the audit rejected this task.

### Your decisions this cycle

You may, in any combination:

1. **`cleanup_dismiss_tasks: [(task_index, reason), ...]`** — bulk-dismiss any subset of Pending tasks. Use this to filter out tasks the audit proposed that you judge not worth the cost (e.g. a Substitution whose audit confidence is `Low` and whose risk of introducing churn outweighs the cleanup benefit). Reviewer-Dismissed is terminal — the task does not resurface.

2. **`cleanup_next_task: Option<task_index>`** — dispatch exactly one Pending task to a worker burst this cycle. The task you select must be Pending. Set `authorized_nodes` to the importers the worker is permitted to edit (Substitution tasks need the full importer set; LintFix needs only the target). For Substitution, the target is **deletable** — the worker will delete `target_node.lean` and `target_node.tex` and rewire importers. For LintFix, the worker is single-file scope. **Substitution tasks must always be dispatched this way — strictly one per burst.**

3. **`cleanup_batch_tasks: [task_index, ...]`** — dispatch MULTIPLE Pending **LintFix** tasks to a SINGLE worker burst, to amortize the ~18-min reviewer/worker burst across several trivial lint-fixes. Guardrails (the kernel rejects the decision otherwise):
   - Batch ONLY `LintFix` tasks, each on a DISTINCT target node, at most `CLEANUP_BATCH_MAX` (= 6) per burst.
   - NEVER batch a Substitution task — dispatch those strictly one per burst via `cleanup_next_task`.
   - Never include a protected-statement node (`live.coverage` ∪ `live.protected_closure_nodes_per_target`).
   - `cleanup_batch_tasks` and `cleanup_next_task` are **mutually exclusive** — set exactly one dispatch mode per decision. A batch of size 1 is legal, but prefer `cleanup_next_task` for a singleton.
   - **Failure coupling:** any single node's checker rejection rolls back the ENTIRE batch atomically. Batch only high-confidence, independent lint-fixes.
   - **Serial fallback:** if a prior batch burst was REJECTED, re-dispatch those specific tasks ONE AT A TIME via `cleanup_next_task` — do not re-batch the same failing set (that would loop). A genuine serial failure then marks that one task Failed, isolating the culprit.
   - On accept, all batched tasks are marked Completed; on reject, all stay Pending (they are not marked Failed) so you can re-dispatch them serially. You do not need to set `authorized_nodes` for a batch — the kernel authorizes exactly the batch's target nodes.

4. **`cleanup_request_reaudit: bool`** — when you decide `Done`, set this true to request another audit round. Legal only if `cleanup_audit_round < 2`. The next round preserves terminal-status tasks but lets the audit revise its own Pending proposals and propose new ones. Use this when round 1 surfaced surprises (e.g. a worker failure that suggests a different substitution).

5. **`Decision::Done`** — finalize the run. The cleanup phase exits into `Phase::Complete`. There is no further work. The run terminates in a fully-formalized state.

### Exit conditions (automatic)

The kernel auto-terminates Cleanup when any of these fire (you don't have to do anything):
- All tasks have reached terminal status (no Pending left).
- `cleanup_consecutive_invalid_workers >= 3` — three consecutive Failed bursts means cleanup isn't converging; finalize.
- Your manual `Done` decision.

In all three cases the run terminates in the same fully-formalized state the cleanup phase entered with.

### Response shape

The only levers that matter in Cleanup are listed above (`cleanup_dismiss_tasks`, `cleanup_next_task`, `cleanup_batch_tasks`, `cleanup_request_reaudit`, `authorized_nodes`, `decision`, `comments`, `reason`). The kernel resolves the worker's active node(s) from the dispatched task's `target_node` (or, for a batch, the batch's target nodes); leave `next_active` empty. The other proof-mode fields (`next_mode`, `reset`, blocker action lists, `allow_new_obligations`, `must_close_active`) are pinned to their cleanup-phase defaults — fill them with whatever the schema shows and don't agonize over them.

### Authority discipline

- The audit proposes tasks. The reviewer triages and dispatches.
- A worker attempts one task per burst via `cleanup_next_task`, OR several independent LintFix tasks per burst via `cleanup_batch_tasks`. There is no in-place retry — a single-dispatch Failed task stays Failed; a rejected batch leaves its tasks Pending for serial re-dispatch.
- The `authorized_nodes` you set is the worker's edit scope. Be precise: too broad invites scope creep; too narrow forces an Invalid response.
- Protected-statement nodes (`live.coverage` ∪ `live.protected_closure_nodes_per_target`) have their Lean signatures and `.tex` statements immutable in cleanup regardless of what you put in `authorized_nodes`. The validator enforces this on the worker output.

Done is always safe. A correct proof is the goal we have already met; cleanup is a nice-to-have, not a must-do.
