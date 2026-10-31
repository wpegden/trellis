## First theorem-stating request: build a real goal/support DAG

Theorem-stating spans many cycles. Build the goal/support DAG from the bottom up using `GOAL.md` and the verified functions' call graph. Author each paired `.lean`/`.tex` goal and support node for the current source and requirements, reusing applicable prior statements and proofs where available. Preserve the byte-pinned extraction-model definitions and import them as leaves.

Before finalizing target statements, inspect the model types in their binders
and results for Rust over-approximation, and inspect the disclosed reachable
opaque inventory in `reachable_opaque_inventory`. Account for panic and
allocation failure, overflow semantics, termination, `unsafe`,
platform-dependent integer widths, and other effects or resource behavior
absent from the model.

Representable Rust-domain restrictions use ordinary validity-definition nodes.
Author each needed validity definition as an ordinary node before staging an
under-model assumption that conditions on it; the assumption-authoring burst
cannot create hooks. Opaque primitive behavior uses the staged under-model
assumption workflow. Follow the common theorem-stating failure modes inlined
elsewhere in this prompt. Representation loss that admits no faithful Lean
predicate stops the run with an explicit unsupported/model-mismatch diagnosis.

- For each target function you address, author its obligations as separate proof-bearing goal nodes with supporting lemma nodes.
- Prefer a rich decomposition that exposes the verification's actual intermediate claims, definitions, and proof structure.
- Isolate reused facts in their own lemma nodes, including facts that are simple in natural language and significant in Lean.
- Add intermediate nodes when they meaningfully decompose a proof obligation.
- Avoid collapsing a multi-step verification into one oversized theorem node with shallow or fake support.
- Keep theorem and lemma statements in proof-bearing nodes.
- Aim for a support graph that later workers and verifiers can understand and extend honestly.

You may defer target functions to later cycles. Each NL proof is individually audited for soundness; on this initial cycle it may begin with `SKETCH:` and be repaired later.
