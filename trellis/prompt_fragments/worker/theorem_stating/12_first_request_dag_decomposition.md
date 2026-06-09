## First theorem-stating request: build a real dependency DAG

This is the first theorem-stating worker request for the run.

Treat it as the moment to lay down a genuinely useful paper-faithful dependency DAG. The richness and correctness of this initial DAG has an outsized effect on the ease of the rest of formazlization process.

- Prefer a rich decomposition that exposes the paper's actual intermediate claims, definitions, and latent proof structure.
- If any arguments are reused in different parts of the paper (even arguments that are simple or trivial in natural language but will be significant from a Lean perspective), these should certainly be isolated in Lemmas, whether or not the original paper makes an analogous presentation choice. 
- Intermediate nodes can be useful even if they will not used in multiple places, so long as they meaningfully decompose an argument.
- Avoid collapsing a multi-step argument into one oversized theorem node with shallow or fake support.
- Don't wrap statements of theorems, lemmas, etc, in definitions; those belong in their own proof-bearing nodes.
- Aim for a support graph that later workers and verifiers can understand and extend honestly.

The goal of this first request is to richly capture the paper's dependency structure so that formalization has a good starting point. 

In some sense, this first cycle is the biggest NL task faced by the whole process, since it NL proofs are supposed to be provided for every proof-bearing node. But don't let this constrain the richness or the DAG you build, or the number of proof-bearing nodes you include. Instead, remember that each NL proof will be individually audited for soundness, giving you extra attempts to repair them one-by-one.  For this reason, at this initial stage only, you should feel comfortable explicitly providing NL proofs that are "sketches", which you can clearly indicate with `SKETCH:` as the first line of the proof.
