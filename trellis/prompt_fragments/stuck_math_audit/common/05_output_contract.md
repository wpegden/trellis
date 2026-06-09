## Audit output

Your response has these fields:

- `report` is required. It is a markdown narrative, at least 200 characters and at most 20,000 characters.
- `cone_clean_node` is optional. Omit it (or leave it null/empty) unless `cone_clean_contract.allowed_nodes` is non-empty; if you set it, the value MUST be drawn from that list. When `cone_clean_contract` is null, no cone clean is legal this dispatch and emitting any value will get the artifact rejected.
- `tasks` is optional. Use zero or more focused `{id, title, body}` objects.
- `probe_paths` is optional. Include paths to useful scratch probes when you used them.
- `global_repair_approve` is required iff `pending_global_repair_request` is non-null; boolean at top level. When true, also set `global_repair_approved_extension_node_ids` to a minimal subset of the reviewer's proposed nodes (drawn from their dependency neighborhood). When false, also set `global_repair_auditor_reason` to a brief decline reason.

Prefer roughly 5-10 high-signal tasks when tasks are useful. A wall of small tasks is worse than a focused list. Combine related work into one task with a structured body.

The report must include at least one concrete signal: a `probe_paths` entry, a fenced code block, or a `## Claim being audited` heading.

If this is a structural-blocker audit, the suggested report shape is:

1. Is there a structural problem with the current strategy?
2. Your falsification attempts, including Lean probes or paper citations.
3. What needs to happen to get to a paper-faithful, successful formalization strategy:
   - specific nodes that need to change
   - how they need to change
   - what work should be done by new helpers, and what work should NOT be done by new helpers

If the previous audit output was rejected, fix the issue before doing anything else:

{{latest_stuck_math_audit_rejection_block}}

Kernel-authored output contract:

{{contract_json}}
