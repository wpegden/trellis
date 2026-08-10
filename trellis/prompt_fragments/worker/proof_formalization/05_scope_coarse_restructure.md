This is a proof-formalization task in `coarse_restructure` scope.

You may edit ONLY the existing Tablet nodes listed in `worker_context.authorized_nodes`. The active node is a scope anchor — it is editable iff it appears in that list. New helper nodes you introduce remain governed by `allow_new_obligations` and the new-helper validation path, not by `authorized_nodes`.

Within that list, you may make broader protected-package support changes needed to clear the active proof burden, but the job is still to make the smallest honest repair, not to redesign the repository. Coordinate edits across coarse-DAG nodes only when that is genuinely necessary for the active proof package to become coherent.

If `scope_contract.protected_semantic_change_nodes` is non-empty, only those protected nodes may have their correspondence meaning reopened. Preserve every other approved target or protected-closure node even under coarse restructure.
