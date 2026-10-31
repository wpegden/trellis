# PV proof-method strategy

These methods close proofs against the Aeneas-generated model. They come from the
tractability spike, where the friction was always the problem-specific bound, never the
mathematics.

**Step through the `Result` monad with `step`.** `step` walks each `let x ← e` binding,
and `step as ⟨ v, hv ⟩` introduces the result variable and its value equation. It
discharges routine no-fail and divisor-nonzero side goals through `scalar_tac`.

**Land a bound as a `have` first.** When `step` cannot discharge a side goal, it is a
problem-specific fact the automation cannot synthesize: a product below the integer
maximum, a modulus result below the modulus, a per-step accumulator below the
whole-collection bound, a quotient in range. State that fact as a `have` (closed by
`Nat.mod_lt`, a prefix-sum split, an integer-division bound, `omega`/`nlinarith` over the
literal input bounds) before the step, so `scalar_tac` picks it up.

**Reuse a contract instead of reproving it.** If a `have` restates a result already proven
over the pinned model in another goal theorem — a model-contract congruence and output
bound, a shared wrapping-arithmetic residue — make it a lemma node and import it rather than
reproving it inline. If necessary for this purpose, consider using `needs_restructure` to
ask the reviewer for wider scope. Write proofs in a modular style initially (e.g., using
`.lean`-closed helper nodes) when it likely serves to avoid such a later restructure.

**Casts.** A widening cast (`U32 → U64`) is `lift`ed and carries an in-bounds obligation;
supply its spec explicitly with `step with UScalar.cast_inBounds_spec` (and
`IScalar.cast_inBounds_spec` for signed). A narrowing cast back is total, so its
obligation is value-preservation under the `< 2^width` bound
(`UScalar.cast_val_mod_pow_of_inBounds_eq`).

**Loops.** Apply `loop.spec_decr_nat` with the measure and invariant given explicitly and
the postcondition left to unify. Passing the postcondition explicitly breaks the `↑r`
versus `r.val` unification. After `split` on the guard, convert the `Bool` guard to its
`.val` form with `simp_all`; the exit branch reduces through `WP.spec_ok`.

**Signed shifts.** Aeneas ships `@[step]` value specs for unsigned (`UScalar`) shifts;
signed (`IScalar`) arithmetic shifts go through a bridge that reduces the shift to integer
floor-division (`BitVec.toInt_sshiftRight` with `Int.shiftRight_eq_div_pow`, valid for
negative operands). Use the curated signed-shift bridges from the support library, and avoid
`bv_decide`/bitblasting.

**Slice access.** Route slice access through the bounds-checked
`Slice.index_usize` (whose spec is real); the unchecked `Slice.get_unchecked` spec is
`sorry`-backed and surfaces `sorryAx`, which the closure probe rejects. Reconcile a signed `%`
between Aeneas's `Int.tmod` postcondition and mathlib's `emod` explicitly when a target needs it.
