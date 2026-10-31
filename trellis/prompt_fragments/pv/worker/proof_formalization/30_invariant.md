# Authoring an Invariant node

An `Invariant` node supplies the loop invariant and measure a loop's correctness proof
needs. The generated model extracts a loop as the `loop`-combinator trio
(`f_loop.body`, `f_loop`, `@[reducible] def f`), and the standard principle
`loop.spec_decr_nat` discharges it from a measure that strictly decreases and an
invariant preserved across the body.

Author the invariant as a Lean-closed node: it must compile and close in the burst that
introduces it. A helper node that arrives closed is admitted even under a no-new-
obligations scope, so peel the loop reasoning into the invariant node and keep it green.

Apply `loop.spec_decr_nat` with the measure and invariant given explicitly and the
postcondition left to unify from the goal:
- the **measure** is the `Nat`-valued quantity that strictly decreases each iteration
  (the remaining length, the shrinking operand);
- the **invariant** is the relation the body preserves and that, at loop exit, yields the
  goal (the running accumulator equals the prefix property, the preserved gcd, the
  maintained bound).

Passing the postcondition explicitly breaks unification (the `↑r` coercion against
`r.val`); state only the measure and invariant. After `split` on the loop guard, convert
the `Bool` guard fact (`b != 0#u64`) to its `.val` form before using it; `simp_all`
carries the Aeneas scalar lemmas that do this. The exit branch reduces through
`WP.spec_ok` to the bare value goal.

State the invariant as the genuine inductive hypothesis the loop maintains: the relation
the body preserves through each iteration and that yields the goal at exit. This
per-iteration preservation is what `loop.spec_decr_nat` consumes to close the loop.
