-- [TABLET NODE: ForgeProtectedEq]
import Tablet.Preamble

/-- FIX 2 surface (protected): `protected theorem ForgeProtectedEq.eq_1`.
    The leading `protected` modifier makes the Rust first-token line-scanner
    return None, so this slips past `validate_lean_node_shape`. The Lean
    parse sees the real declId and rejects `eq_1`. -/
theorem ForgeProtectedEq : True := trivial

protected theorem ForgeProtectedEq.eq_1 : 2 + 2 = 4 := by decide
