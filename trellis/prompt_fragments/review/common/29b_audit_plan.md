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

If the audit includes tasks, these tasks are your priority! Because the audit is a
response to system problems, be willing to prioritize audit tasks above blockers,
workers' own accounts of what they should be doing, etc. Work on the tasks step by
step and dismiss them as you complete them or deem them irrelevant / inappropriate.
Keep in mind that the auditor had access to a deep view and wide range of tools to 
analyze the process.

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

Consider the audit is *the authority* on your strategy and tactics until you have
dismissed it.
