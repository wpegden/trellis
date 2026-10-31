## Target orientation (theorem-stating)

The supervisor's goal is proving the configured verification targets — the worker-authored goal theorems (`Spec`, `Safety`, `Correctness`, `Invariant`) over the byte-pinned `ExtractionModel` def nodes. The model defs are fixed leaves the worker never edits; theorem-stating builds the goal-theorem package above them (specs, safety obligations, invariants) that proof-formalization will later close. Your job here is to keep that package **substantive and correctly stated over the pinned model** and well-decomposed {{theorem_target_orientation_timing}}. Coverage means the goal theorems cover the declared function targets. Theorem-stating may span many cycles: accept a deliberately partial DAG. When the only violation is that a target is not yet covered, prioritize the solidity of the goal statements over resolving it.

{{parallel_target_orientation_guidance}}

In theorem-stating, broader theorem work — including creating new helper nodes and editing the signatures of existing nodes — happens by re-issuing the worker in `next_mode: Global` with explicit comments naming the structural change you want. `Global` authorizes broad theorem-stating edits: new helpers, signature edits, dependency rewiring. After cycle 1, it still does not authorize new proof-bearing nodes with `SKETCH:` NL proofs; workers must either provide complete NL proofs expected to pass strict soundness verification or avoid creating those nodes.

Choose `advance_phase` (when the contract's `allowed_decisions` offers it) once the goal-theorem package is settled: every declared verification target is covered by goal theorems stating its `GOAL.md` contract at full strength over the pinned model, those statements are substantive and the verifier lanes are clean, and you judge the package ready for proof-formalization to begin closing it. Until then, keep refining statements under `continue`.

Pivoting to an unrelated node whose failure is not what's blocking the current focus is almost never the right move. Those nodes will still be there later, and will often be easier once the current statement package is settled.
