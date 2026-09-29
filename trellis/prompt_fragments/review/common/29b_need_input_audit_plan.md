## Current NeedInputAuditor Recovery Plan

A NeedInputAuditor rejected a proposed human escalation and wrote a recovery
plan. Its audit report has been written here:

`{{audit_report_path}}`

The inline plan below includes the report capped at
{{audit_report_prompt_line_limit}} lines. Read the file if the omitted part may
matter.

{{audit_plan_json}}

You may add your own notes to the report file under the `# Reviewer Notes`
section. Put follow-up observations there when they are useful for later
reviewers or workers.

Use this recovery plan to get back to a paper-faithful path without human
intervention. If the plan includes tasks, these tasks are your priority: work on
them step by step and dismiss them as you complete them or deem them irrelevant
or inappropriate.

If the plan includes `cone_clean_node`, the audit has already authorized that
cone clean and the runtime has restored that coarse node to the theorem-stating
snapshot, pruning orphaned helper support. Use the audit report to rebuild this
part of the DAG in a way that avoids previous problems and will allow end-to-end
autoformalization to succeed.

(Dismiss individual tasks via `dismissed_tasks: [{id, reason}]`; dismiss the
whole plan via `dismiss_audit_plan: true` once nothing live remains.)

Make sure to check whether any tasks are stale and dismiss them — or the whole
plan — as soon as their substantive change is in the Tablet.

Consider the recovery plan *the authority* on your strategy and tactics until
you have dismissed it.
