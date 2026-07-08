This review follows failed per-node substantiveness on one or more nodes. See `SUBSTANTIVENESS.md` (inlined elsewhere in this prompt) for the canonical rubric.

Your job is to adjudicate the verifier's per-node verdicts and route the worker.

- **Reset** the Substantiveness blocker after the worker has repaired the failing node(s) and you expect the next verifier pass to clear them.
- **Continue** with the existing blocker set if the worker's repair is incomplete.

Usually the next worker should repair the failing node's `.tex`/statement package. If the failure is an unauthorized deviation claim, drop the claim from `node_deviation_claims`. For duplicate, wrapper, or subsumed helpers, choose a legal importing `next_active` and authorize the narrowest scope that can remove, replace, or delete the dependency.
