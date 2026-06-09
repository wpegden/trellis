## Target validity constraints

Every `target_node` in `new_tasks` must satisfy:

1. **Present**: the node must be in `current_present_nodes`. Don't propose tasks against nodes that don't exist (or have been deleted by an earlier task in this round).
2. **Not protected**: the node must NOT be in `protected_statement_node_set`. The protected set is the union of:
   - the covering nodes for every configured target (`live.coverage`), and
   - the per-target type-level closure (`live.protected_closure_nodes_per_target`).
   These are the user-stated theorems and their structural support — their Lean signatures and `.tex` statements are immutable in cleanup.

If your target is in the protected set, the kernel will reject the task in validation and you'll get one retry per burst before the kernel forces `audit_done` early.

### Substitution replacement validity

For `Substitution { replacement: TabletWrapper(N) }`:
- N must be in `current_present_nodes`.
- N may itself be in the protected-statement set (replacements *may* be protected; only the *target* of a substitution may not).

For `Substitution { replacement: Mathlib(citation) }`:
- The citation must be non-empty.
- The kernel does NOT verify that the cited mathlib lemma exists or that it's actually applicable. The worker discovers that when it tries to inline the replacement. If you're wrong about a mathlib citation, the worker burst will fail Invalid and consume the consecutive-invalid budget.

### LintFix validity

For `LintFix { warning_text }`:
- The warning text must be non-empty.
- Single-node scope: the worker is restricted to editing `target_node.lean` only. Do not propose LintFix tasks whose fix requires editing multiple files.

### Duplicate detection

The kernel rejects `(target_node, kind)` pairs that duplicate an existing task in `cleanup_audit_tasks` (any status). If you've already proposed `Substitution(A → Foo)` in burst 1, you may not propose the same `(A, Substitution)` pair again — even if you've decided the replacement should be `Bar` instead. Either:
- Use `task_modifications` to dismiss the prior proposal first, then propose the new one in a subsequent burst, OR
- Propose a Substitution against a different target.
