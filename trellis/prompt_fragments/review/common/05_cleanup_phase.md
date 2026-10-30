## Cleanup phase (v2 — task-driven)

Formalization is done. The audit has produced a list of cleanup tasks (`cleanup_audit_tasks` in the request). Each task is one of:
- **Substitution** — delete a wrapper node and inline its replacement (a mathlib lemma or another tablet node) at every importer.
- **LintFix** — single-node hygiene edit driven by a lake-build warning.
- **DeadCodeElim** — remove mechanically-dead proof lines from one node; acceptance requires the node to remain closed and become strictly shorter.
- **ExtractHelper** — split a proof-internal helper lemma out of an oversized node, one helper per burst.
- **ExtractShared** — replace a duplicated proof block across its declared parents with one shared helper; the worker may convert any subset of at least two.

Each task has a status:
- **Pending** — eligible for dispatch or dismissal.
- **Completed** — a worker burst applied this task successfully.
- **Failed** — a worker burst attempted this task and failed (single attempt; not retried).
- **Dismissed** — you or the audit rejected this task.

### Worker acceptance contract

- **Substitution** deletes exactly the target source/`.tex` pair. Every authorized source that previously referenced the target names the declared replacement afterward, and tablet-node replacements update each `\noderef` to that node. Retained declarations and protected manuscript statements stay invariant.
- **LintFix** edits every dispatched target source and removes that member's pre-burst named diagnostic. A diagnostic already absent at the pre-burst observation is reported as a stale task. Single and batch dispatches land atomically.
- **DeadCodeElim** preserves the node declaration and manuscript while producing a closed proof with fewer source lines.
- **ExtractHelper** creates exactly one source/`.tex` helper pair per parent, preserves each parent declaration and manuscript statement, shortens each parent, closes every helper in the same burst, and proves genuine parent use through fresh closure records.
- **ExtractShared** creates one source/`.tex` helper pair, converts at least two declared parents, preserves and shortens every changed parent, proves genuine use through fresh closure records, and decreases the changed-parent-plus-helper line sum.

Every new helper appears explicitly in `target_claim_updates` with `[]`, and in `challenge_claim_updates` with `[]` when challenge targets are configured. The acceptance identity in the request identifies these judging rules.

### Your decisions this cycle

You may, in any combination:

1. **`cleanup_dismiss_tasks: [{"task_index": 0, "reason": "concise reason"}, ...]`** — bulk-dismiss any subset of Pending tasks. Use one object per task with exactly the `task_index` and `reason` fields. Reviewer-Dismissed is terminal.

2. **`cleanup_next_task: Option<task_index>`** — dispatch exactly one Pending task to a worker burst this cycle. The task you select must be Pending. Set `authorized_nodes` to the importers the worker is permitted to edit (Substitution tasks need the full importer set; LintFix and DeadCodeElim need only the target; ExtractShared derives its entire scope from the task). For Substitution, the target is **deletable** — the worker will delete `target_node.lean` and `target_node.tex` and rewire importers. LintFix and DeadCodeElim are single-file scope. **Substitution, DeadCodeElim, and ExtractShared tasks must always be dispatched this way — strictly one per burst and never combined with another dispatch.**

   When both ExtractShared and ExtractHelper are Pending, dispatch ExtractShared first. A successful dedup may shrink parents enough that their later ExtractHelper tasks should be dismissed. Each ExtractShared task carries `region_block_lines`, the shared-block length the kernel measured for its declared parents at proposal time (absent when the parent set matched no scanned region — treat that as low priority). Compute `recoverable = region_block_lines × (n_parents − 1)` with `n_parents = |{target_node} ∪ co_parents|`; prefer greater `recoverable`, breaking comparable values toward smaller `region_block_lines`. Tasks over the same or overlapping parent sets target one region — dispatch at most one of them per round.

3. **`cleanup_batch_tasks: [task_index, ...]`** — dispatch several Pending tasks of one kind to a single worker burst, to amortize the ~18-min reviewer/worker burst across them. Guardrails (the kernel rejects the decision otherwise):
   - Batch `LintFix` or `ExtractHelper` tasks, one kind per burst, each on a distinct target node, at most `CLEANUP_BATCH_MAX` (= 6) per burst. DeadCodeElim is not batchable initially because its worker loop can require many compile attempts. ExtractShared is never batchable because one task already owns its whole parent set.
   - An ExtractHelper batch asks one worker for several independent decompositions in one burst.
   - Dispatch Substitution tasks one per burst via `cleanup_next_task` — a burst carries one edit envelope, and there is none for several substitutions at once.
   - Never include a protected-statement node (`live.coverage` ∪ `live.protected_closure_nodes_per_target`).
   - `cleanup_batch_tasks` and `cleanup_next_task` are **mutually exclusive** — set exactly one dispatch mode per decision. A batch of size 1 is legal, but prefer `cleanup_next_task` for a singleton.
   - **Failure coupling:** any single node's checker rejection rolls back the entire batch atomically. Batch only tasks you are confident in and whose targets are independent of one another.
   - **Serial fallback:** if a prior batch burst was rejected, re-dispatch those specific tasks one at a time via `cleanup_next_task`. A genuine serial failure then marks that one task Failed, isolating the culprit.
   - On accept, all batched tasks are marked Completed; on reject, all stay Pending (they are not marked Failed) so you can re-dispatch them serially. You do not need to set `authorized_nodes` for a batch — the kernel authorizes exactly the batch's target nodes.

4. **`cleanup_request_reaudit: bool`** — when you decide `Done`, set this true to request another audit round. Legal while `cleanup_audit_round < max_rounds`; your contract carries both values and the derived `request_reaudit_legal`. The next round preserves terminal-status tasks but lets the audit revise its own Pending proposals and propose new ones. Use this when the round just finished surfaced surprises (e.g. a worker failure that suggests a different substitution), or when its completed work changed the corpus enough that a fresh scan is worth taking.

5. **`cleanup_repair_node: Option<NodeId>`** — dispatch a worker to repair one node's correspondence. Cleanup can carry an open NodeCorr blocker on a node outside the protected surface: an extracted helper is corr-Unknown at birth, and becomes corr-Fail when the verifier finds its prose and its Lean disagree. This is the lever that clears it. The worker edits that node's `.tex` statement block and nothing else; its `.lean` stays byte-identical. Mutually exclusive with `cleanup_next_task` and `cleanup_batch_tasks` — one dispatch mode per decision.

   A corr blocker is outstanding work: `Done` stays illegal while one stands, so clearing it is the path to finishing.

6. **`Decision::Done`** — finalize the run. Legal when no verifier lane blocker is outstanding. While a corr blocker stands, `Done` is rejected and `cleanup_repair_node` is the path to clearing it. On acceptance the cleanup phase exits into `Phase::Complete` and the run terminates in a fully-formalized state.

### Exit conditions

The kernel finishes Cleanup on your `Done`, and automatically when a Continue leaves no Pending task and no outstanding verifier work. Three consecutive Failed bursts latch `cleanup_force_done`, which stops further task dispatch rather than ending the phase.

ExtractShared is the most demanding burst. Stop dispatching further ExtractShared tasks after two consecutive dedup failures rather than risking the force-done latch; repeated failure may indicate worker configuration or strengthening difficulty, not an impossible region. A wide successful dedup also has a long acceptance because each changed parent receives a fresh closure probe, so schedule it accordingly.

For ExtractShared, `Completed` means at least two declared parents were converted. It does not promise that the region is clean: compare `swept_parents` with `target_node ∪ co_parents`. A rejected wide task landed nothing and may be proposed later on a narrower parent set. A partially swept Completed task is different: do not dispatch a second extraction over its leftovers, because the existing helper makes that a semantic duplicate — and no task kind converts a leftover parent to the existing helper, so its remaining copies are accepted debt.

In every case the run terminates in the same fully-formalized state the cleanup phase entered with.

### Response shape

The cleanup levers are `cleanup_dismiss_tasks`, `cleanup_next_task`, `cleanup_batch_tasks`, `cleanup_request_reaudit`, `cleanup_repair_node`, `authorized_nodes`, `decision`, `comments`, and `reason`. The kernel resolves the worker's active node(s) from the dispatched task's `target_node` or batch targets; leave `next_active` empty. Fill the remaining proof-mode fields with the defaults shown by the schema.

### Authority discipline

- The audit proposes tasks. The reviewer triages and dispatches.
- A worker attempts one task per burst via `cleanup_next_task`, or several independent LintFix or ExtractHelper tasks via `cleanup_batch_tasks`. A single-dispatch Failed task stays Failed; a rejected batch leaves its tasks Pending for serial re-dispatch.
- The `authorized_nodes` you set is the worker's edit scope. Be precise: too broad invites scope creep; too narrow forces an Invalid response.
- Protected-statement nodes (`live.coverage` ∪ `live.protected_closure_nodes_per_target`) have their Lean signatures and `.tex` statements immutable in cleanup regardless of what you put in `authorized_nodes`. The validator enforces this on the worker output.

The cleanup semantics are designed to ensure we always have a faithful and correct formalization, but Done should also require that we have no verifier lane blockers.
