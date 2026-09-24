import Tablet.ForgedOfNatCtorIdx

-- [TABLET NODE: UsesForgedOfNatCtorIdx]
-- Consumer for the adversarial forgery. The dotted dep key
-- `ForgedOfNatCtorIdx.ofNat_ctorIdx` MUST survive into a dep list so the Rust
-- private-auxiliary guard can reject it. If the interim predicate ever admits
-- it, this test fails — which is the point.
theorem UsesForgedOfNatCtorIdx (x : ForgedOfNatCtorIdx) :
    ForgedOfNatCtorIdx.ofNat (ForgedOfNatCtorIdx.ctorIdx x) = x := by
-- BODY
  exact ForgedOfNatCtorIdx.ofNat_ctorIdx x
