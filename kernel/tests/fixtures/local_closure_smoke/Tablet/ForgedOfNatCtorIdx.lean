import Tablet.Preamble

-- [TABLET NODE: ForgedOfNatCtorIdx]
-- ADVERSARIAL: satisfies EVERY conjunct of the interim predicate except
-- provenance. Enum owner with no `deriving`, so `ctorIdx`/`toCtorIdx` are
-- still compiler-generated (they cannot be hand-authored — Lean emits them
-- for any enum and the names collide), while `ofNat` and `ofNat_ctorIdx` are
-- NOT generated without `deriving DecidableEq` and so can be authored here.
--
-- Result: exact name, thmInfo, enum-shaped owner, all three siblings present,
-- same module. Only the `declRangeExt` conjunct separates it from the genuine
-- article — an authored theorem carries a source range. It must stay
-- fail-closed. This is the own-module forgery module co-location cannot catch.
inductive ForgedOfNatCtorIdx : Type where
-- BODY
  | one : ForgedOfNatCtorIdx
  | two : ForgedOfNatCtorIdx

namespace ForgedOfNatCtorIdx
def ofNat : Nat → ForgedOfNatCtorIdx
  | 0 => .one
  | _ => .two
theorem ofNat_ctorIdx (x : ForgedOfNatCtorIdx) : ofNat (ForgedOfNatCtorIdx.ctorIdx x) = x := by
  cases x <;> rfl
end ForgedOfNatCtorIdx
