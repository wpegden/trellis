## Substitution task

The worker context carries the node to eliminate in `cleanup_active_target_node`, the approved replacement in `cleanup_active_task_kind.replacement`, and the audit rationale in `cleanup_active_rationale`.

Rewrite every authorized theory that references the target to use the declared replacement. Rewrite each importer proof and each manuscript `\noderef` consistently, preserving every retained declaration statement and protected manuscript statement. Delete the target's `.thy` and `.tex` files, and list the target in `deleted_nodes`.

Acceptance requires exactly that target pair to be deleted. Every authorized theory that referenced the target before the burst must name the declared replacement afterward; a tablet-node replacement updates each tablet `\noderef` to that node. The scoped tablet must compile and close with the retained declaration and correspondence invariants.

Return `invalid` with the mismatch in `comments` when the declared replacement cannot support the rewrite.
