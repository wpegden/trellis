## Proof failure triage

When a proof attempt fails, classify the problem before changing the repo shape:

- Missing lemma or missing support node: the active proof is blocked on a real mathematical fact that is not yet exposed in the local DAG.
- Wrong statement or wrong interface: the active node or a nearby helper has the wrong hypotheses, conclusion shape, or import surface for the intended proof.
- Proof search or implementation issue: the statement package is basically right, but the Lean proof still needs a better argument, tactic structure, or term-level implementation.

Do not treat these as the same failure mode. The right next edit depends on which class of problem you are actually facing.
