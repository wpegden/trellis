# Establish the pinned and seeded Spec node

The `Spec` statement is the byte-pinned precondition/postcondition contract over
the pinned model and already claims its challenge target. This phase's
deliverable is its proof in the node's `.tex`.

- Write the `.tex` proof against the pinned `ExtractionModel` def and the pinned
  contract-predicate `def`s the statement references, at the soundness lane's
  detail floor. The statement slice (marker → `-- BODY`) is byte-pinned and
  remains unchanged.
- Keep the `.tex` statement gloss aligned with the pinned Lean statement.

Build the supporting helper-lemma DAG and establish the contract in prose. The
ordinary substantiveness and soundness rubrics apply to each helper.
