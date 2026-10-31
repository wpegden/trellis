## GapResearch Planner output

- `report` (required, 200–20,000 chars): how the route plugs the gap and
  what crate facts and pinned-model machinery it leans on; when escalating, the
  precise question for a human.
- `route_tex`: the natural-language proof-route — prose describing how to
  close the gap, citing current Tablet nodes/obligations where motivating.
  Required unless `route_needs_human` is set.
- `route_needs_human`: set when no route exists that the pinned model supports
  and the goal property is genuinely unprovable against it without escalation;
  put the question and ruled-out alternatives in `report`.

Aim for the minimal correct fix given the crate, `GOAL.md`, the pinned model,
and the tablet state.

If the previous output was rejected, fix the issue before anything else:

{{latest_stuck_math_audit_rejection_block}}

Kernel-authored output contract:

{{contract_json}}
