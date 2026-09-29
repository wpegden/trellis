## Audit output

Your response has these fields:

- `confirm_need_input` is required. Set it true only if you confirm a real
  fundamental paper problem or paper/tablet impossibility requiring human
  review.
- `report` is required. It is a markdown narrative, at least 200 characters and
  at most 20,000 characters.
- `tasks` is required when `confirm_need_input` is false. Use focused
  `{id, title, body}` objects that describe how to get back to a
  paper-faithful path.
- `probe_paths` is optional. Include paths to useful scratch probes when you
  used them.

If `confirm_need_input` is true, tasks are optional and should only support the
human escalation report. If `confirm_need_input` is false, include at least one
recovery task.

Prefer roughly 5-10 high-signal tasks when tasks are useful. A wall of small
tasks is worse than a focused list. Combine related work into one task with a
structured body.

The report must include at least one concrete signal: a `probe_paths` entry, a
fenced code block, or a `## Claim being audited` heading.

If the previous audit output was rejected, fix the issue before doing anything
else:

{{latest_stuck_math_audit_rejection_block}}

Kernel-authored output contract:

{{contract_json}}
