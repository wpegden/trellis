## GapResearch Planner output — trust protocol v1

- `report` (required, 200–20,000 chars): explain the proposed legal route, or
  for terminal refusal identify the obstruction and the routes ruled out.
- `route_tex`: a natural-language autonomous proof/repair route. It may direct
  a validated typed refutation to structured target-bound adjudication, or an
  unbound target through statement binding first. Required unless
  `route_needs_human` is set.
- `route_needs_human`: terminal-refusal signal only. Set it when no autonomous
  proof/repair, structured adjudication, or statement-binding route is legal.
  The kernel writes a halt sentinel. It does not open a HumanGate.

A carrier, reviewer statement, or Sound result selects routing only. It never
becomes a station record, give-up, or polarity decision.

If the previous output was rejected, fix the issue before anything else:

{{latest_stuck_math_audit_rejection_block}}

Kernel-authored output contract:

{{contract_json}}
