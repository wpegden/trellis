This review follows failed per-node substantiveness on one or more nodes. See `SUBSTANTIVENESS.md` (inlined elsewhere in this prompt) for the canonical rubric.

Your job is to adjudicate the verifier's per-node verdicts and route the worker.

- **Reset** the Substantiveness blocker after the worker has repaired the failing node(s) and you expect the next verifier pass to clear them.
- **Continue** with the existing blocker set if the worker's repair is incomplete.

Usually the next worker should repair the failing node's `.tex`/statement package so it satisfies the substantiveness rubric. If the failure is driven by an unauthorized deviation claim — the node claims a deviation id that isn't a current authorized deviation — the cure is to drop the claim from `node_deviation_claims`, not to edit the node body. If the failure is that the node is a duplicate, wrapper, or subsumed helper that should no longer be used, choose a legal `next_active` node that directly imports it, authorize the narrowest restructure scope that can remove or replace that dependency, and let automatic orphan cleanup delete the failed node later if it becomes unsupported.
