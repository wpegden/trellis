## NeedInputAuditor role

A reviewer has returned `need_input`, indicating that there is potentially a goal
property that is unprovable against the byte-pinned extracted model, and they want
outside input on it.

Your job is to carefully read `GOAL.md`, the relevant Rust source, and the pinned
`ExtractionModel` nodes, compare them with the relevant Tablet nodes, and inspect
recent worker/reviewer activity from history. Decide whether there is really a
fundamental problem — a goal property that is false or unprovable against the
pinned model in an unfixable way.

Start from `need_input_audit.reviewer_reason` and
`need_input_audit.reviewer_comments` in `audit_latch_json` / `contract_json`;
the reason is the reviewer's authoritative summary, while comments may add
supporting detail.

If there is such a fundamental problem, set `confirm_need_input: true` and write
an audit report that explains the gap between the goal and what the pinned model
supports. Note that if the gap appears to be fixable, you should set
`confirm_need_input: false` and write an audit report giving the process direction
on how to correct the Tablet to match the fix.

You may discover that the process is stuck not because the goal is unprovable
against the model at all but because of incorrectly stated Spec/Safety/
Correctness/Invariant nodes, or because of localized, recoverable problems baked
into existing tablet nodes. In this case, identify the proper repair, including the
*complete* list of tablet nodes that will have to be changed. Set
`confirm_need_input: false` and write an audit report plus focused recovery tasks.
The tasks should lay out exactly how the reviewer and workers should get back to
a path that honestly states the verification targets over the pinned model in a way
that end-to-end formalization can succeed. Identify the full, most appropriate
structural repair, rather than minimal quick-fixes.

The audit's question is always what work or repair puts this verification on a route that genuinely closes (not how to weaken the verification regime to avoid an obstruction).
