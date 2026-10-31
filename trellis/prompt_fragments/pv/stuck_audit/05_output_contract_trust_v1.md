## Audit output

Your response has these fields:

- `report` is required. It is a markdown narrative, at least 200 characters and at most 20,000 characters.
- `cone_clean_node` is optional. Omit it (or leave it null/empty) unless `cone_clean_contract.allowed_nodes` is non-empty; if you set it, the value MUST be drawn from that list. When `cone_clean_contract` is null, no cone clean is legal this dispatch and emitting any value will get the artifact rejected.
- `tasks` is optional. Use zero or more focused `{id, title, body}` objects.
- `probe_paths` is optional. Include paths to useful scratch probes when you used them; prior cycles' `.trellis/stuck-math-audit/cycle-*/` artifacts are citable as read-only evidence.
- `global_repair_approve` is required iff `pending_global_repair_request` is non-null; boolean at top level. When true, also set `global_repair_approved_extension_node_ids` to a minimal subset of the reviewer's proposed nodes (drawn from their dependency neighborhood). When false, also set `global_repair_auditor_reason` to a brief decline reason.
- `memory_operations` is optional: the run's durable process memory, materialized by the kernel under `process-memory/` and surfaced to every later worker, reviewer, verifier, and audit as settled knowledge. Record each route this audit refuted and each constraint, interface decision, or counterexample it established as an `add` (the kernel assigns the `pm-...` id); `supersede` an active entry whose claim changed, and `retire` one shown faulty with evidence. Keep each body self-contained so it survives rewinds. Adjudicate every entry under any "Pending memory challenges" in this prompt.

Prefer roughly 5-10 high-signal tasks when tasks are useful. A wall of small tasks is worse than a focused list. Combine related work into one task with a structured body.

Prescribe only work a worker can land under the process's construction rules: file and DAG shape follow `FILESPEC.md`, deletion happens only where the checker permits, and imports must record real logical dependencies rather than artificial edges added only to satisfy the orphan checker. In PV mode, a standalone support node may land while the PRE-burst state still has a configured theorem target that is uncovered or carries only a `SKETCH:` NL proof. Once every configured theorem target is present and carries a real NL proof, every new node must be imported by an existing or same-burst consumer. When a pivot needs a construction no present node can yet consume after that window closes, route it through a restructure or a target refutation.

The report must include at least one concrete signal: a `probe_paths` entry, a fenced code block, or a `## Claim being audited` heading.

If this is a structural-blocker audit, the suggested report shape is:

1. Is there a structural problem with the current strategy?
2. Your falsification attempts, including reduced obligations or citations to the crate (`crate/src/lib.rs`) / the pinned goal targets / the pinned model.
3. What needs to happen to get to a successful, faithful-to-the-goal formalization strategy:
   - specific nodes that need to change
   - how they need to change
   - what work should be done by new helpers, and what work should NOT be done by new helpers

The assumption-authoring lane is closed on trust protocol v1: leave
`under_model_candidate_invariant` empty. Route a dropped language guarantee
by the classification in the adjudication instructions.

If the previous audit output was rejected, fix the issue before doing anything else:

{{latest_stuck_math_audit_rejection_block}}

Kernel-authored output contract:

{{contract_json}}
