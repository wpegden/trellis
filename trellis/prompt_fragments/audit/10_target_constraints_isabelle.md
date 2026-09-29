## Target validity constraints

Every `target_node` in `new_tasks` must satisfy:

1. **Present**: the node must be in `current_present_nodes`. Don't propose tasks against nodes that don't exist (or have been deleted by an earlier task in this round).
2. **Not protected**: the node must NOT be in `protected_statement_node_set`. The protected set is the union of:
   - the covering nodes for every configured target (`live.coverage`), and
   - the per-target type-level closure (`live.protected_closure_nodes_per_target`).
   These are the user-stated theorems and their structural support — their Isabelle statements and `.tex` statements are immutable in cleanup.

If your target is in the protected set, the kernel will reject the task in validation and you'll get one retry per burst before the kernel forces `audit_done` early.

### Substitution replacement validity

For `Substitution { replacement: TabletWrapper(N) }`:
- N must be in `current_present_nodes`.
- N may itself be in the protected-statement set (replacements *may* be protected; only the *target* of a substitution may not).

For a `Substitution` whose replacement is an Isabelle library citation:
- The citation must be non-empty.
- The kernel does NOT verify that the cited library theorem exists or that it's actually applicable. The worker discovers that when it tries to inline the replacement. If you're wrong about a citation, the worker burst will fail Invalid and consume the consecutive-invalid budget.

### LintFix validity

For `LintFix { warning_text }`:
- The warning text must be non-empty.
- Single-node scope: the worker is restricted to editing `target_node.thy` only. Do not propose LintFix tasks whose fix requires editing multiple files.

### Duplicate detection

The kernel rejects a proposed task that matches an existing task in `cleanup_audit_tasks` (any status) on the same `target_node` with the same **full kind value** — the kind name together with its payload. A Substitution with a different replacement or a LintFix with different warning text is a distinct kind value and legal immediately; no prior dismissal is needed (though dismissing a superseded Pending proposal via `task_modifications` keeps the list tidy).
