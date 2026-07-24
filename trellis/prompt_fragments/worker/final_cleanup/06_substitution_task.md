## Cleanup-v2 Substitution task

This burst targets the elimination of a redundant tablet node by inlining a replacement (either a mathlib lemma or another tablet node that subsumes it). The audit identified this node as a wrapper or shallow alias that adds no mathematical content beyond the replacement.

### What you must do

The active task has:
- **`target_node`** — the tablet node to delete.
- **`kind.replacement`** — either a mathlib citation (e.g. `Nat.add_comm`) or a tablet `node` to inline in its place.
- **`rationale`** — the audit's free-form note on why this substitution is safe.

Your burst should:

1. **Rewrite every importer** of `target_node` to inline the replacement. If the replacement is a mathlib lemma, replace `target_node` references with the mathlib citation (importing `Mathlib...` as needed). If the replacement is another tablet node, replace `target_node` references with the wrapped node, and adjust any argument differences. The importer list is everything in `authorized_nodes` minus the target.
2. **Sweep `\noderef{target_node}` references** in `.tex` files belonging to the importers and replace each with the equivalent reference to the replacement. These are mechanical reference updates — no soundness re-verification is triggered by the cleanup-phase validator for these `.tex` edits.
3. **Delete `target_node.lean` and `target_node.tex`**. Both files. The validator expects exactly these two deletions; any other deletion is rejected.
4. **Re-prove each importer's `LocalClosureRecord`**. Touched importers need their closure record re-derived after the inline rewrite. The standard post-burst pipeline runs `apply_local_closure_acceptance_bookkeeping`; you don't have to invoke it directly, but your edits must compile and close all imports.
5. **Submit the raw artifact** in the usual final-cleanup format.

### What you must NOT do

- Do not modify the **`.tex` statement** of any node in the `protected_statement_node_set`. The `\noderef` sweep is allowed on *other* nodes' `.tex` files; protected-statement nodes' `.tex` is immutable. The validator runs `paper_target_corr_reopen_guard_report_with_scope` against the protected set and rejects any sweep that incidentally re-fingerprints a protected statement.
- Do not modify the **Lean signature** (declaration hash) of any non-target authorized node. Proof bodies of importers may be rewritten freely; their signatures must remain invariant. The validator enforces decl-hash invariance for `protected_statement_node_set ∪ (authorized_nodes \ {target_node})`.
- Do not edit nodes outside `authorized_nodes ∪ {target_node}`. The authorized scope is exactly the importers the reviewer pre-scoped for this burst.
- Do not introduce new tablet nodes. Substitution is strictly subtractive plus rewires.
- Do not change the target's replacement strategy mid-burst. If the proposed replacement turns out to not work (e.g. argument order mismatch you can't bridge), submit an Invalid response — the task is marked Failed and the consecutive-invalid counter advances. Do not improvise an alternative replacement.

### Acceptance

The kernel re-checks `formalization_complete()` after the burst:
- No new sorrys, no new global blockers.
- All importers compile.
- Decl-hashes invariant where required.

On any failure the burst is rejected and the task is marked Failed. There is no in-place retry; the next cycle's reviewer decides whether to dispatch a different task, dismiss the rest, or finalize.
