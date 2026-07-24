## Kernel context

Repository root:

`{{repo_path}}`

The current StuckMathAudit latch:

{{audit_latch_json}}

If present, this is the immediately previous audit plan — dismissed, cleared, or (on a scheduled planner burst) still live. Read it before drafting. Re-validate each carried-forward task against the current Tablet state, rewriting its framing to match what should happen now; drop any the current state has already overtaken:

{{previous_audit_plan_snapshot_json}}

Current request summary:

{{request_summary_json}}

Read `latest_worker_rationale` and `reviewer_comments` within it — the blocked worker's own reasoning, often the specific calculation or counterexample driving this audit.

Project invariants:

{{project_invariants_json}}

Filespec:

`{{filespec_path}}`
