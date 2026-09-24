-- [TABLET NODE: UsesIndPredInduction]
import Tablet.Preamble
import Tablet.IndPredReach

/-- FIX B regression consumer: proved by STRUCTURAL RECURSION over the
    recursive inductive predicate `IndPredReach`. The recursive call's Nat
    argument INCREASES (`m + 1`), so the equation compiler cannot recurse
    structurally on the Nat — it must recurse on the predicate evidence,
    which for a Prop-valued recursive inductive goes through the
    `IndPredBelow`-generated `IndPredReach.brecOn` / `IndPredReach.below`
    (UNtagged by `markAuxRecursor`). The elaborated proof term therefore
    retains those constants.

    Pre-Fix-B: `Lean.isAuxRecursor` misses them → recorded as dotted dep
    keys (`IndPredReach.brecOn`) → kernel Patch C-K fail-closes →
    `internal_error`. Post-Fix-B: `isRecursorFamilyRealization` (suffix +
    inductive parent + module co-location) transparent-walks them; the
    real invalidation edge `IndPredReach` is still recorded as a strict
    definition dep. -/
theorem UsesIndPredInduction : ∀ m n, IndPredReach m n → m ≤ n
  | _, _, .refl n => Nat.le_refl n
  | _, _, .step m n h => Nat.le_of_succ_le (UsesIndPredInduction (m + 1) n h)
