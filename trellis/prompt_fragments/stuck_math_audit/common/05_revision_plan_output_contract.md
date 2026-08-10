## Revision plan output

- `report` (required, 200–20,000 chars): the mathematical delta, the per-target
  classification, and the minimal update route.
- `tasks`: focused `{id, title, body}` objects through the route (roughly 5–10).
- `revision_actions`: the structured plan the kernel routes into revision state.
  - `targets`: one entry per changed, added, or removed configured target:
    - `target`: a configured target from `revision_planning.target_deltas`;
    - `classification`: `add` or `removed` (following the paper diff), or
      `strengthen` (your judgment on a changed target);
    - `covering_nodes`: the nodes covering this target after the route.
  - `nodes`: one entry per node whose disposition changes — the route verbs:
    - `node`: with `action: new`, the name you give the node you author; with
      every other action, the exact name of a node present in `revision_planning`
      (frozen or editable). For `copy_as_new_node`, `node` is the present SOURCE
      you clone — give the clone its own name and role in that node's task body,
      and list that new name in the added target's covering_nodes.
    - `action`: `freeze` (carry untouched), `restate` (edit to the new claim),
      `reprove` (re-derive under a changed dependency), `copy_as_new_node` (clone
      a present node's stack under a new name to cover an added target), `retire`
      (drop a node whose target is gone), or `new` (author a node the new
      mathematics needs).
    - `reason`: a brief justification.
  - Check each `node` against `revision_planning` before emitting: a present name
    takes any action; a name that is not present is valid only with `action:
    new`.
- `probe_paths`: paths to scratch probes you used.

If the previous output was rejected, fix the issue before anything else:

{{latest_stuck_math_audit_rejection_block}}

Kernel-authored output contract:

{{contract_json}}
