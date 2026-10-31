## Current Plan

A planning burst charted how this phase should unfold.
Its report has been written here:

`{{audit_report_path}}`

The inline plan below includes the report capped at
{{audit_report_prompt_line_limit}} lines. Read the file if the omitted part may
matter.

{{audit_plan_json}}

You may add your own notes to the report file under the `# Reviewer Notes`
section. Put follow-up observations there when they are useful for later
reviewers or workers.

Use the plan to choose useful tasks and dismiss each as it is completed or
becomes irrelevant. Its task order is advisory; continue useful compatible work
in parallel within the native task scopes and gates. The planner read the full paper and configured
targets; treat the plan as the default strategy while it remains live.

Workers keep their normal authority over decomposition; the plan guides which
definitions and lemmas to lay down first and the order in which targets are
reached.

(Dismiss individual tasks via `dismissed_tasks: [{id, reason}]`; dismiss the whole
plan via `dismiss_audit_plan: true` once nothing live remains.)

Check whether any tasks are stale and dismiss them — or the whole plan — as soon
as their substantive change is in the Tablet.
