-- [TABLET NODE: UsesCtorSizeOfSpec]
import Tablet.Preamble
import Tablet.InductiveNat

/-- FIX B `sizeOf_spec` clause fixture: the proof term explicitly retains
    the constructor `sizeOf` specification theorem
    `InductiveNat.mk.sizeOf_spec`, which Lean generates eagerly (in
    `InductiveNat`'s own module) under the CONSTRUCTOR's namespace and
    does NOT tag via any environment predicate reachable from the probe.
    Pre-Fix-B it was recorded as a dotted dep key
    (`InductiveNat.mk.sizeOf_spec`) → kernel Patch C-K fail-closes.
    Post-Fix-B `isCtorSizeOfSpecTheorem` (constructor parent + module
    co-location) transparent-walks it; the real invalidation edge
    `InductiveNat` is still recorded as a strict definition dep.

    The binder's type is deliberately INFERRED (not pinned to `Nat`) so
    this consumer keeps compiling when the `#[ignore]`d Patch C-K Fix 2
    mutation test flips `InductiveNat.mk` to `Bool → InductiveNat` and
    rebuilds the fixture. -/
theorem UsesCtorSizeOfSpec :
    ∀ n, sizeOf (InductiveNat.mk n) = 1 + sizeOf n :=
  InductiveNat.mk.sizeOf_spec
