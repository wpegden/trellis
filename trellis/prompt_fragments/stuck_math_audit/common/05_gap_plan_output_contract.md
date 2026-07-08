## GapResearch Planner output

- `report` (required, 200–20,000 chars): how the route plugs the gap and
  what paper machinery it leans on; when escalating, the precise question for
  a human.
- `route_tex`: the natural-language proof-route — prose describing how to
  close the gap, citing current Tablet nodes/obligations where motivating.
  Required unless `route_needs_human` is set.
- `route_needs_human`: set when no faithful route exists and the gap needs
  new mathematics from a human; put the question and ruled-out alternatives
  in `report`.

Aim for the minimal correct fix given the paper and the tablet state. A route
may rest on a deviation where that is the minimal faithful fix; describe it in
`route_tex` and the worker authors it through the normal deviation path
(DEVIATIONS.md).

If the previous output was rejected, fix the issue before anything else:

{{latest_stuck_math_audit_rejection_block}}

Kernel-authored output contract:

{{contract_json}}
