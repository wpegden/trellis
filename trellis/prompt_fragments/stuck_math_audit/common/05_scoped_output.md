Return `report`, `tasks`, and `probe_paths` using the contract below. Give only the tasks needed for the named goals. Audit priorities guide the reviewer; they do not prohibit compatible work elsewhere. This audit supplies a plan, not permission to reset nodes, change statements or polarity, or advance a human gate.

The report must include a concrete signal: a probe path, a fenced code block, or a `## Claim being audited` heading. Make findings self-contained; scratch files alone are not lasting recovery evidence.

Optional `memory_operations` use these forms:

- `{"op":"add", "type":"refuted-route", "coarse_node":"GoalA", "title":"Short title", "body":"Self-contained claim and evidence"}`
- `{"op":"supersede", "entry_id":"pm-...", "type":"constraint", "title":"Updated title", "body":"Updated claim and evidence"}`
- `{"op":"retire", "entry_id":"pm-...", "reason":"Why the claim is faulty, with evidence"}`

Other entry types are `interface-decision`, `counterexample`, and `process-note`. Add entries under a named coarse goal. Supersede or retire only entries in those goals' supplied process memory. Include the evidence in the entry body. Shared entries require a run-wide audit. Address the supplied memory challenges in the report.

{{latest_stuck_math_audit_rejection_block}}

{{contract_json}}
