## Kernel context

Repository root:

`{{repo_path}}`

The current StuckMathAudit latch:

{{audit_latch_json}}

If present, this is the immediately previous audit plan — dismissed, cleared, or (on a scheduled planner burst) still live. Read it before drafting. Re-validate each carried-forward task against the current Tablet state, rewriting its framing to match what should happen now; drop any the current state has already overtaken:

{{previous_audit_plan_snapshot_json}}

Current request summary:

{{request_summary_json}}

Read `latest_worker_rationale` and `reviewer_comments` within it — the blocked worker's own reasoning, often the specific calculation or counterexample driving this audit.

Project invariants:

{{project_invariants_json}}

Filespec:

`{{filespec_path}}`

Per-node lane verdicts:

`{{context_json_path}}`

The `node_lane_verdicts` object in that file carries the kernel's current verdict for each present node, populated when its `available` field is `true`:

- `lean_closure`, keyed by proof node: `closed`, `open`, `failed`, `unverified`. Every other node kind carries no Lean-closure obligation and is absent from this map.
- `correspondence` and `substantiveness`: `pass`, `fail`, `?` (the lane owes this node a verdict).
- `soundness`: `pass`, `fail`, `?`, and `not_required` for a node whose Lean is closed, which discharges its soundness obligation.
- `sound_recorded_fail`, keyed by the nodes carrying a soundness fail on record: the name of that verdict, `ReviewerPinnedFail` among them. A node reading `not_required` in the `soundness` map appears here when the Soundness lane rejected it before its Lean closed.
