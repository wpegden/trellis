This review follows failed per-node substantiveness on one or more nodes. See `SUBSTANTIVENESS.md` (inlined elsewhere in this prompt) for the canonical rubric.

Your job is to adjudicate the verifier's per-node verdicts and route the worker.

- **Omit** the Substantiveness blocker after the worker has repaired the failing node's `.tex`: the repair changes the node's fingerprint, so the recorded Fail reverts to Unknown on its own — leave the blocker out of every action list and hand the package back for verification; the next verifier pass clears it. Repaired-to-Unknown blockers never appear in `allowed_reset_ids`, so no reset is needed or legal for them.
- **Reset** (`reset_blocker_ids`, discards a current Fail to Unknown) only a blocker the contract lists in `allowed_reset_ids`: a still-binding Fail on unchanged node text that you judge a fresh verifier pass should re-adjudicate. When `allowed_reset_ids` is empty, no reset is legal this transition.
- **Continue** with the existing blocker set if the worker's repair is incomplete.

Usually the next worker should repair the failing node's `.tex`/statement package. If the failure is an unauthorized deviation claim, drop the claim from `node_deviation_claims`. For duplicate, wrapper, or subsumed helpers, choose a legal importing `next_active` and authorize the narrowest scope that can remove, replace, or delete the dependency.
