import Tablet.EnumDerivedDecEq

-- [TABLET NODE: UsesEnumDerivedDecEqOfNatCtorIdx]
-- A LEGAL consumer: its proof pulls the compiler-generated
-- `EnumDerivedDecEq.ofNat_ctorIdx` into the closure. The source names it
-- explicitly so elaboration cannot optimize the dependency away; a real run
-- reaches it transitively through `decide`/`DecidableEq`.
theorem UsesEnumDerivedDecEqOfNatCtorIdx (x : EnumDerivedDecEq) :
    EnumDerivedDecEq.ofNat (EnumDerivedDecEq.ctorIdx x) = x := by
-- BODY
  exact EnumDerivedDecEq.ofNat_ctorIdx x
