## Cleanup-v2 LintFix task

This burst targets a single lake-build warning on a single tablet node. The audit captured the verbatim warning text in the task's `warning_text` field; your job is to edit `target_node.lean` to eliminate that warning.

### What you must do

The active task has:
- **`target_node`** — the tablet node to edit.
- **`kind.warning_text`** — the verbatim lake-build warning to resolve.
- **`rationale`** — the audit's note (usually a one-line description).

Your burst should:

1. **Edit `target_node.lean`** to resolve the warning. Typical fixes: drop an unused variable, rename a shadowed binder, narrow an unused import, replace a deprecated lemma name, etc.
2. **Keep the Lean signature invariant**. The declaration hash of `target_node` must match its baseline post-edit — change the proof body only, not the type signature. The validator checks decl-hash invariance and rejects signature drift.
3. **Submit the raw artifact** in the usual final-cleanup format.

### What you must NOT do

- Do not modify the **`.tex` file** of the target or any other node. LintFix is Lean-side only.
- Do not edit any node other than `target_node`. The authorized scope is exactly the single target.
- Do not create or delete any tablet files.
- Do not change the target's signature. If the warning seems to require a signature change, submit an Invalid response — that warning belongs in a Substitution or restructure task, not a LintFix.

### Acceptance

The kernel re-checks `formalization_complete()` after the burst:
- No new sorrys, no new global blockers.
- Tablet still compiles.
- Decl-hash for `target_node` matches baseline.
- Correspondence fingerprint for `target_node` matches baseline (proof-body edits don't reach the .tex statement, so this is automatic if you followed the rules above).

On any failure the burst is rejected and the task is marked Failed.
