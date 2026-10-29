## Proof failure triage

When a proof attempt fails, classify the problem before changing the repo shape:

- Missing lemma or missing support node: the active proof is blocked on a real mathematical fact that is not yet exposed in the local DAG.
- Wrong statement or wrong interface: the active node or a nearby helper has the wrong `assumes`/`shows`, conclusion shape, or import surface for the intended proof.
- Proof search or implementation issue: the statement package is basically right, but the Isar proof still needs a better argument, method, or term-level implementation.

Do not treat these as the same failure mode. The right next edit depends on which class of problem you are actually facing.
