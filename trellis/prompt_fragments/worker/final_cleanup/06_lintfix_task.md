## Cleanup-v2 LintFix task

This burst targets one or more lake-build warnings, each on its own tablet node. The audit captured the verbatim warning text for each task; your job is to edit each authorized `target_node.lean` to eliminate its warning.

- In the **single-task** case the request carries one active task (`cleanup_active_target_node_view` / `cleanup_active_task_kind_view.warning_text` / `cleanup_active_rationale_view`).
- In the **batch** case the request carries `cleanup_active_batch_view` — a list of `(target_node, warning_text)` pairs. Fix every one of them in this single burst. The authorized edit scope is exactly the set of these target nodes (and, in the single-task case, the one active target node).

### What you must do

For **each** authorized target node:

1. **Edit `target_node.lean`** to resolve its warning. Typical fixes: drop an unused variable, rename a shadowed binder, narrow an unused import, replace a deprecated lemma name, etc.
2. **Keep the Lean signature invariant**. The declaration hash of each target node must match its baseline post-edit — change the proof body only, not the type signature. The validator checks decl-hash invariance for every changed node and rejects signature drift.
3. **Submit the raw artifact** in the usual final-cleanup format.

### What you must NOT do

- Do not modify the **`.tex` file** of any node. LintFix is Lean-side only.
- Do not edit any node other than the authorized target nodes. The authorized scope is exactly those nodes — nothing else.
- Do not create or delete any tablet files.
- Do not change any target's signature. If a warning seems to require a signature change, leave that node's `.lean` untouched — that warning belongs in a Substitution or restructure task, not a LintFix.

### Acceptance

The kernel re-checks `formalization_complete()` after the burst and re-verifies every changed node exactly as for a single-node fix:
- No new sorrys, no new global blockers.
- Tablet still compiles.
- Decl-hash for each changed node matches baseline.
- Correspondence fingerprint for each changed node matches baseline (proof-body edits don't reach the `.tex` statement, so this is automatic if you followed the rules above).

Acceptance is whole-burst atomic: if any single node fails a check, the ENTIRE burst is rejected and rolled back. In the batch case the rejected tasks stay Pending (they are not marked Failed) so the reviewer can re-dispatch them one at a time. In the single-task case a rejected task is marked Failed.
