-- [TABLET NODE: ForgeBareEq]
import Tablet.Preamble

/-- FIX 2 surface (bare): a bare top-level `theorem ForgeBareEq.eq_1`
    authored alongside the principal. Reserved-shaped final component
    `eq_1` must be rejected at this node's own acceptance. (Principal is
    declared first so its reserved equation-lemma slot stays free for the
    authored `eq_1`.) -/
theorem ForgeBareEq : True := trivial

theorem ForgeBareEq.eq_1 : 2 + 2 = 4 := by decide
