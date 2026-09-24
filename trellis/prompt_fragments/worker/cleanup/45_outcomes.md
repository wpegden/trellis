## Worker outcome guidance

Use `valid` when the cleanup attempt is a serious acceptable cleanup under the current scope.

Every live node has target support or an import path from target support.

Use `invalid` when the cleanup cannot be accepted. Set `invalid_kind` to `implementation_invalid`, `task_infeasible`, `checker_rejected`, or `contract_conflict`, and put the concrete evidence in `comments`.
