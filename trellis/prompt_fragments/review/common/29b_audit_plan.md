## Current Audit Plan

An audit was triggered because of possible problems making genuine progress.
Its audit report has been written here:

`{{audit_report_path}}`

The inline plan below includes the report capped at
{{audit_report_prompt_line_limit}} lines. Read the file if the omitted part may
matter.

{{audit_plan_json}}

You may add your own notes to the report file under the `# Reviewer Notes`
section. Put follow-up observations there when they are useful for later
reviewers or workers.

The audit serves as a counterbalance to an occasional tendency of the normal
reviewer-worker loop to favor moves that look like local progress when a larger
structural issue has been baked in that is blocking actual progress toward
formalization. Use the audit report to get back on track.

Prioritize the audit's mathematical findings and necessary repairs. Its task
order is advisory: continue useful compatible work in parallel, including
closure attempts on other goals. Dismiss tasks as you complete them or find them
irrelevant. The auditor's broader view can reveal problems missed by workers.

If the plan includes `cone_clean_node`, the audit has already authorized that
cone clean and the runtime has restored that coarse node to the theorem-stating
snapshot, pruning orphaned helper support. Use the audit report to rebuild this
part of the DAG in a way that avoids previous problems and will allow end-to-end
autoformalization to succeed.

Remember that workers can have a tendency to prefer to pick up easy, bite-size tasks 
when a fundamental structural issue looms. Don't let the worker's response to audit 
itself take this form as well; that very tendency may well be why the audit was necessary.

(Dismiss individual tasks via `dismissed_tasks: [{id, reason}]`; dismiss the whole
plan via `dismiss_audit_plan: true` once nothing live remains.)

Make sure to check whether any tasks are stale and dismiss them — or the whole
plan — as soon as their substantive change is in the Tablet.

Remember that the point of the audit is that it can detect broader problems. Do
not hesitate to authorize broader scope when broader changes are necessary.

Use the audit as the default strategy while it remains live. Its scheduling
advice creates no additional dependencies or permissions; follow the native task
scopes and gates.
