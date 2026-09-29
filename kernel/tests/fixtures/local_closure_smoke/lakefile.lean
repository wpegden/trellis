import Lake
open Lake DSL

package «tablet» where
  leanOptions := #[
    ⟨`autoImplicit, false⟩
  ]

@[default_target]
lean_lib «Tablet» where
  srcDir := "."

-- Module-system fixtures cannot be imported by the legacy `Tablet.lean`
-- library root. Keep them as an explicit default target so a clean `lake
-- build` still creates every exported/server/private artifact used by the v2
-- falsifier suite.
@[default_target]
lean_lib «SplitFixtures» where
  srcDir := "."
  roots := #[`Tablet.SplitVisibility, `Tablet.SplitVisibilityConsumer]

-- The third-party-style deriving handlers live OUTSIDE the `Tablet`
-- namespace on purpose: the generated declarations they emit land in the
-- deriving module, so only exact module metadata can attribute them. Root
-- modules are not part of `lean_lib Tablet`, so they need their own target
-- or `import WeirdDeriveSupport` cannot resolve.
lean_lib «WeirdDeriveSupport» where
  srcDir := "."
  roots := #[`WeirdDeriveSupport]

-- This fixture intentionally does NOT pull in Mathlib. Our trivial
-- proofs (`True := trivial`, `True := by sorry`) only need the
-- standard prelude. Operators who want to extend the fixture with
-- a Mathlib-using node must add `require mathlib from git ...` and
-- run `lake exe cache get` before building.
