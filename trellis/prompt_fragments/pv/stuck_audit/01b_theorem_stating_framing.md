In theorem-stating phase, look for broader structural problems — over-strengthened or monolithic statements, goal theorems that do not honestly state the property over the pinned model.

Be sure to *carefully read `GOAL.md` and the relevant Rust source* to understand what changes need to be made so that the goal theorems cover the declared verification targets at honest strength over the byte-pinned model, and end-to-end verification can succeed.

Theorem-stating is expected to span many cycles, built from the bottom up. Treat a higher verification target that is simply not yet covered as expected build-up, and focus on whether the goal statements (`Spec`, `Safety`, `Correctness`, `Invariant`) are correctly stated over the pinned model and sound.
