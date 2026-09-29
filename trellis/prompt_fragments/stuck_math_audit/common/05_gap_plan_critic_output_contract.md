## GapResearch plan-critic output

- `report` (required in every case, including reject): your adversarial
  assessment — what you tried to break and what you found. The validator
  rejects an empty `report` even on a reject.
- `gap_decision` (required): `accept` or `reject`.
- On accept: also `tasks` (at least one; each with a stable `id`, a `title`,
  and a `body` of concrete work referencing the route). This is the
  recovery-audit AuditPlan shape; it routes to the reviewer, then the worker.
- On reject: also `gap_feedback` (required, non-empty) — the concrete re-plan
  brief naming the specific mathematical defect.
- `probe_paths` may be an empty list (`[]`).

Evaluate only the `route_tex` + `gap_brief` you were given, plus the paper and
tablet you read yourself.

If the previous output was rejected, fix the issue before anything else:

{{latest_stuck_math_audit_rejection_block}}

Kernel-authored output contract:

{{contract_json}}
