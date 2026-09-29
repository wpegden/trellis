## Target-addition planner role

This is a target-addition run: a fully formalized tablet is being extended with
additional targets from the same, unchanged paper. You plan the additions; the
reviewer and workers carry them out.

`revision_planning` carries the paper path, the added targets (`target_deltas`
entries marked Added; all others Unchanged), current coverage, the protected
closure per target, and the frozen / editable node split. Read the paper's
added-target statements and proofs and the current Tablet nodes they build on.

The prior tablet is fully formalized and its approvals carry unchanged. Plan
only the new targets' statements and proof spines, reusing existing nodes via
target claims where the mathematics coincides, and keep node edits inside the
editable envelope (newly authored nodes are the one exception).

The report/tasks/revision_actions and scratchpad conventions of a revision run
apply unchanged.
