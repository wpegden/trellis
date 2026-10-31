# Authoring a Correctness node

A `Correctness` node proves the functional correctness theorem: under the spec's
preconditions, the model function returns `ok` with a result satisfying the
postcondition. State it as a Hoare triple `f args ⦃ ret => P ret ⦄` whose subject `f`
is the pinned `ExtractionModel` declaration imported into this node.

State the theorem over the pinned model declaration directly. The theorem's subject must
resolve to the generated def of the intended function, so that correctness is certified
about the extracted model. Import the model node and
unfold the generated declaration to begin the proof.

Drive the proof with `step`: it walks the `Result`-monad `let x ← e` bindings, introduces
each result variable and its value equation (`step as ⟨ v, hv ⟩`), and discharges the
routine no-fail side goals through `scalar_tac`. For a loop, apply `loop.spec_decr_nat`
with the measure and invariant from the loop's `Invariant` node. Carry values in `Nat`
or `Int` through the `.val` projection and finish the arithmetic with `omega`, `scalar_tac`,
or `nlinarith` once the bounds are in context.

When a step's side goal is a problem-specific bound the automation cannot find, land it
as a `have` first (the cast-back range, the per-step accumulator bound, the quotient
range), then take the step. Reuse the cross-node facts you need from the spec node, the
invariant node, and the curated support lemmas. Keep the
proof inside the approved axiom surface, and confirm `#print axioms` reports exactly
`[propext, Classical.choice, Quot.sound]` before submitting.
