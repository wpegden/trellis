## Audit output

Your response has these fields:

- `report` is required. It is a markdown narrative, at least 200 characters and at most 20,000 characters.
- `cone_clean_node` is optional. Omit it (or leave it null/empty) unless `cone_clean_contract.allowed_nodes` is non-empty; if you set it, the value MUST be drawn from that list. When `cone_clean_contract` is null, no cone clean is legal this dispatch and emitting any value will get the artifact rejected.
- `tasks` is optional. Use zero or more focused `{id, title, body}` objects.
- `probe_paths` is optional. Include paths to useful scratch probes when you used them.
- `global_repair_approve` is required iff `pending_global_repair_request` is non-null; set it to true or false at top level. When true, also set `global_repair_approved_extension_node_ids` to a minimal subset of the reviewer's proposed nodes (drawn from their dependency neighborhood). When false, also set `global_repair_auditor_reason` to a brief decline reason.
- `node_retirement_request` is optional: `{"nodes": [...], "reason": "<why>"}` orders the reviewer to dispatch deletion of the named nodes as the next worker task. Legal on a proof-formalization plan-writing audit; each node must be present, outside `coarse_dag_nodes` (a coarse node routes through `cone_clean_node`), and outside the approved-target/challenge protection set. Emit at most one of `node_retirement_request` / `cone_clean_node`. A reviewer decline is surfaced as `latest_node_retirement_decline`.
- `memory_operations` is optional: the run's durable process memory, materialized by the kernel as files under `process-memory/` and surfaced to every later worker, reviewer, verifier, and audit as settled knowledge. Record each route this audit refuted and each constraint it established:
  - `{"op": "add", "type": "refuted-route" | "constraint" | "interface-decision" | "counterexample" | "process-note", "coarse_node": "<coarse node id or \"global\">", "title": "<short title>", "body": "<the claim>"}` — new entry (the kernel assigns the `pm-...` id).
  - `{"op": "supersede", "entry_id": "pm-...", "type": "...", "title": "...", "body": "..."}` — replace an active entry whose claim changed; the old file remains as a tombstone.
  - `{"op": "retire", "entry_id": "pm-...", "reason": "<why the entry is faulty, with evidence>"}` — mark an active entry faulty; the file remains as a tombstone.

  Bodies must be self-contained: inline the counterexample, the refutation sketch, or the paper citation (section/display) — `.trellis/` probe and chat paths are advisory decoration that does not survive rewinds. When your finding reinforces a lesson an active entry already records, `supersede` that entry with a strengthened body that folds in the new instance. Mutating an entry outside your own coarse node is allowed when the report states why. Adjudicate every entry listed under "Pending memory challenges" in this prompt: retire or supersede the entry when the challenger's evidence holds (citing that evidence), and state in the report why each rejected challenge fails.

Prefer roughly 5-10 high-signal tasks when tasks are useful. A wall of small tasks is worse than a focused list. Combine related work into one task with a structured body.

The report must include at least one concrete signal: a `probe_paths` entry, a fenced code block, or a `## Claim being audited` heading.

If this is a structural-blocker audit, the suggested report shape is:

1. Is there a structural problem with the current strategy?
2. Your falsification attempts, including Isabelle probes or paper citations.
3. What needs to happen to get to a paper-faithful, successful formalization strategy:
   - specific nodes that need to change
   - how they need to change
   - what work should be done by new helpers, and what work should NOT be done by new helpers

If the previous audit output was rejected, fix the issue before doing anything else:

{{latest_stuck_math_audit_rejection_block}}

Kernel-authored output contract:

{{contract_json}}
