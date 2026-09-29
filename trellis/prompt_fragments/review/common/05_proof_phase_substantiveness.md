This review follows failed per-node substantiveness on one or more nodes. See `SUBSTANTIVENESS.md` (inlined elsewhere in this prompt) for the canonical rubric.

Your job is to adjudicate the verifier's per-node verdicts and route the worker.

A substantiveness blocker is cleared by the verifier passing on the node.

- **Continue** and task the next worker to repair the failing node's `.tex`/statement package so the next substantiveness pass clears it; keep the existing blocker set until it does.
- Name the node's blocker in `reset_blocker_ids` when a change since the verdict — typically a repair in an importing node — means the node would now pass; the reset puts the node back on the substantiveness verifier frontier, where a later cycle picks it up.

If the failure is an unauthorized deviation claim, drop the claim from `node_deviation_claims`. For duplicate, wrapper, or subsumed helpers, choose a legal importing `next_active` and authorize the narrowest scope that can remove, replace, or delete the dependency.
