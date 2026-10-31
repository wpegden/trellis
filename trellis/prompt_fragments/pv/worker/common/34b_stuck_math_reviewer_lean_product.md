## StuckMathAudit reviewer product

The reviewer is in `StuckMathAudit` mode and produced a diagnostic product
for this worker burst. Use it as context, but verify its mathematical and Lean
claims against the crate, `GOAL.md`, the pinned model, the current Tablet state,
and the usual worker contract before relying on it.
If the product includes a `scratch_file` path, that file is on disk and
readable; `cat` it to inspect the reviewer's argument directly (route any Lean
check through the worker/checker pipeline, not a host `lake`).

Diagnostic product:

{{reviewer_lean_product_json}}
