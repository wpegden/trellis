-- [TABLET NODE: UsesCtorInjEq]
import Tablet.Preamble
import Tablet.InductiveNat

/-- Consumer whose proof closure references the constructor-generated theorem
    `InductiveNat.mk.injEq`. The local-closure collector should
    transparent-walk that theorem and record only the principal
    `InductiveNat` dependency. -/
theorem UsesCtorInjEq : (InductiveNat.mk 0 = InductiveNat.mk 1) = (0 = 1) := by
  exact InductiveNat.mk.injEq 0 1
