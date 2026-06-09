import Lake
open Lake DSL

package «tablet» where
  leanOptions := #[
    ⟨`autoImplicit, false⟩
  ]

@[default_target]
lean_lib «Tablet» where
  srcDir := "."

-- This fixture intentionally does NOT pull in Mathlib. Our trivial
-- proofs (`True := trivial`, `True := by sorry`) only need the
-- standard prelude. Operators who want to extend the fixture with
-- a Mathlib-using node must add `require mathlib from git ...` and
-- run `lake exe cache get` before building.
