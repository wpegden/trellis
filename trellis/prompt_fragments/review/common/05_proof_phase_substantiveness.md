This review follows failed per-node substantiveness on one or more nodes. See `SUBSTANTIVENESS.md` (inlined elsewhere in this prompt) for the canonical rubric.

Your job is to adjudicate the verifier's per-node verdicts and route the worker.

Here a substantiveness blocker is cleared by the verifier passing on a repaired node, not by a reviewer reset — `blocker_actions.allowed_reset_ids` is empty here.

- **Continue** and task the next worker to repair the failing node's `.tex`/statement package so the next substantiveness pass clears it; keep the existing blocker set until it does.

If the failure is an unauthorized deviation claim, drop the claim from `node_deviation_claims`. For duplicate, wrapper, or subsumed helpers, choose a legal importing `next_active` and authorize the narrowest scope that can remove, replace, or delete the dependency.
