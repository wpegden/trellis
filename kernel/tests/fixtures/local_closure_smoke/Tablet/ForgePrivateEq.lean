-- [TABLET NODE: ForgePrivateEq]
import Tablet.Preamble

/-- FIX 2 surface (private): `private theorem ForgePrivateEq.eq_1`. Same
    line-scanner bypass as `protected`. -/
private theorem ForgePrivateEq.eq_1 : 2 + 2 = 4 := by decide

theorem ForgePrivateEq : True := trivial
