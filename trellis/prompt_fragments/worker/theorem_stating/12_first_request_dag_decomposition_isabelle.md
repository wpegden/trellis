## First theorem-stating request: build a real dependency DAG

This is the first theorem-stating worker request for the run.

Theorem-stating is expected to span many cycles. Build the support DAG from the bottom up: on this cycle, establish the paper's foundational definitions and lowest-layer lemmas and make them paper-faithful and sound. It is fine if higher statements and paper targets are deferred to later cycles once this layer is solid.

- Prefer a rich decomposition that exposes the paper's actual intermediate claims, definitions, and latent proof structure.
- If any arguments are reused in different parts of the paper (even arguments that are simple or trivial in natural language but will be significant from an Isabelle perspective), these should certainly be isolated in lemmas, whether or not the original paper makes an analogous presentation choice.
- Intermediate nodes can be useful even if they will not used in multiple places, so long as they meaningfully decompose an argument.
- Avoid collapsing a multi-step argument into one oversized theorem node with shallow or fake support.
- Don't wrap statements of theorems, lemmas, etc., in definitions; those belong in their own proof-bearing nodes.
- Aim for a support graph that later workers and verifiers can understand and extend honestly.

You do not need to cover every paper target on this cycle. For the foundational nodes you do state, each NL proof is individually audited for soundness; at this initial stage you may give a sketch proof, marked with `SKETCH:` as its first line, to be repaired over later cycles.
