## No New `SKETCH:` Nodes After Cycle 1

Starting with cycle 2, do not create a new proof-bearing tablet node whose `.tex` proof has a `SKETCH:` marker. If you add a new theorem, lemma, corollary, or helper node, you must write a complete NL proof that, even after your own adversarial self-audit against `SOUNDNESS.md`, you believe will pass a strict soundness verifier.

If you cannot do that, do not create the node. This cycle-specific rule overrides the general `SKETCH:` guidance for newly created proof-bearing nodes, and the worker checker rejects violations deterministically.
