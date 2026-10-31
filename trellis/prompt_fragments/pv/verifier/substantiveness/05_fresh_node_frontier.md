Review every node in `request_summary.nodes` against `SUBSTANTIVENESS.md`, `GOAL.md`, the tablet, and the byte-pinned extraction model.

Emit one explicit `Pass`, `FalseAsStated`, `Fail`, or `NotDoneYet` verdict per node. Comments are required for `FalseAsStated` and `Fail`. The kernel reissues nodes marked or defaulted to `NotDoneYet`.
