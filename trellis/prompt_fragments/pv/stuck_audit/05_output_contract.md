## Audit output

Your response has these fields:

- `report` is required. It is a markdown narrative, at least 200 characters and at most 20,000 characters.
- `cone_clean_node` is optional. Omit it (or leave it null/empty) unless `cone_clean_contract.allowed_nodes` is non-empty; if you set it, the value MUST be drawn from that list. When `cone_clean_contract` is null, no cone clean is legal this dispatch and emitting any value will get the artifact rejected.
- `tasks` is optional. Use zero or more focused `{id, title, body}` objects.
- `probe_paths` is optional. Include paths to useful scratch probes when you used them; prior cycles' `.trellis/stuck-math-audit/cycle-*/` artifacts are citable as read-only evidence.
- `global_repair_approve` is required iff `pending_global_repair_request` is non-null; boolean at top level. When true, also set `global_repair_approved_extension_node_ids` to a minimal subset of the reviewer's proposed nodes (drawn from their dependency neighborhood). When false, also set `global_repair_auditor_reason` to a brief decline reason.
- `memory_operations` is optional: the run's durable process memory, materialized by the kernel under `process-memory/` and surfaced to later roles as audited findings. Record supported constraints, interface decisions, and counterexamples, including their scope and evidence as an `add` (the kernel assigns the `pm-...` id); `supersede` an active entry whose claim changed, and `retire` one shown faulty with evidence. Keep each body self-contained so it survives rewinds. Adjudicate every entry under any "Pending memory challenges" in this prompt.

Give only the tasks needed to resolve the diagnosed obstruction, ordered by dependency.

Recommend the scope needed to resolve the obstruction. The reviewer must obtain any required scope change through the ordinary process before assigning edits; `FILESPEC.md` governs the resulting artifacts. The uncovered-paper-target construction window permits temporary orphan nodes; otherwise new support needs an existing or same-burst consumer. The structured request determines which construction and repair actions are available.

The report must include at least one concrete signal: a `probe_paths` entry, a fenced code block, or a `## Claim being audited` heading.

If your plan's route to progress is admitting a narrow PV under-model
premise (a dropped Rust guarantee or a missing binding to the already-selected
extraction environment), do BOTH:
describe it in a task AND put the precise candidate statement in the structured
`under_model_candidate_invariant` response field. Only the structured field is
enactable — the reviewer cannot open the assumption-authoring lane from task
prose, and can never restate your candidate. Leave the field empty otherwise.

For an environment binding, identify the accepted scope and primary evidence
connecting it to the model primitive. Weigh a minimal binding against available
proof routes. It must leave formalization convincing evidence about the selected
Rust behavior. The same environment governs both goal polarities; changing the
GOAL's hypotheses or replacing the frozen extraction requires a separate scope
decision. This route is available during modeling, stating, and formalization.

If the previous audit output was rejected, fix the issue before doing anything else:

{{latest_stuck_math_audit_rejection_block}}

Kernel-authored output contract:

{{contract_json}}
