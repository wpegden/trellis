-- [TABLET NODE: UsesCtorInjEq]
import Tablet.Preamble
import Tablet.InductiveNat

/-- Consumer whose proof closure references the constructor-generated theorem
    `InductiveNat.mk.injEq`. The local-closure collector should
    transparent-walk that theorem and record only the principal
    `InductiveNat` dependency.

    The statement quantifies over the constructor's argument (its type is
    INFERRED from `InductiveNat.mk`) instead of using `Nat` literals, so —
    like `UsesInductive`, which documents the same requirement — this node
    still compiles when the ctor-mutation test flips `mk : Nat → …` to
    `mk : Bool → …` (literals `0`/`1` would need `OfNat Bool _` and break
    the mutant `lake build` now that this module is registered in the
    fixture root). -/
theorem UsesCtorInjEq :
    ∀ a b, (InductiveNat.mk a = InductiveNat.mk b) = (a = b) := by
  intro a b
  exact InductiveNat.mk.injEq a b
