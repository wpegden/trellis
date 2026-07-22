-- [TABLET NODE: IndPredReach]
import Tablet.Preamble

/-- FIX B fixture: a RECURSIVE Prop-valued inductive PREDICATE. For this
    shape `Lean.Meta.IndPredBelow` eagerly generates `IndPredReach.below`
    (an `inductDecl`) and `IndPredReach.brecOn` (a `thmDecl`) in this
    module WITHOUT `markAuxRecursor` tagging, so `Lean.isAuxRecursor` is
    false for them in every environment. A consumer proved by structural
    induction over this predicate (see `UsesIndPredInduction.lean`)
    retains `.brecOn` / `.below` in its proof term; pre-Fix-B the probe
    recorded them as dotted dep keys, which the kernel's Patch C-K
    present-node validation fail-closes on → spurious `internal_error`.

    The `step` constructor counts UP (from `m` toward `n`), so a recursive
    consumer cannot fall back to structural recursion on the Nat argument
    — only the predicate itself decreases, forcing the equation compiler
    through `IndPredReach.brecOn`. -/
inductive IndPredReach : Nat → Nat → Prop where
  | refl : ∀ n, IndPredReach n n
  | step : ∀ m n, IndPredReach (m + 1) n → IndPredReach m n
