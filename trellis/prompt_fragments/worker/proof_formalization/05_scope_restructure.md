This is a proof-formalization task in `restructure` scope.

You may edit ONLY the existing Tablet nodes listed in `worker_context.authorized_nodes`. The active node is a scope anchor — it is editable iff it appears in that list. New helper nodes you introduce are governed by `allow_new_obligations` and the new-helper validation path, not by `authorized_nodes`.

Use the authorized list to repair the active node's local support surface: add or refactor helper nodes (new), edit existing helpers in the list, adjust their imports, or make other local edits genuinely needed to clear the active proof burden. Keep changes tied to the active node rather than drifting into unrelated cleanup.

Every helper is a separate Tablet node: a new `.lean` file with exactly one principal declaration matching its node name, plus a matching `.tex` file. Do not add extra top-level declarations inside the active node's own `.lean` file.

`restructure` lets you change the active node's signature only if the active node is not in `scope_contract.coarse_dag_nodes`, meaning it was introduced during proof-formalization rather than approved during theorem-stating. Signature repairs on coarse-DAG nodes require `coarse_restructure`; if you need that broader scope, return `needs_restructure`.
