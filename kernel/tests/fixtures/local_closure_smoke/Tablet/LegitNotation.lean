-- [TABLET NODE: LegitNotation]
import Tablet.Preamble

/-- FIX 3 no-false-positive: term-level `notation` and `infix` are ALLOWED.
    They introduce term-level syntax whose RHS is a `termParser`, so they
    cannot expand to a top-level declaration and cannot synthesize a
    reserved-shaped constant. A worker may legitimately want operator
    notation, so the owner-file scan must NOT flag this node. The principal
    `LegitNotation` is a normal theorem. -/
notation:max "⟦" x "⟧" => x

infixl:65 " ⊕nat " => Nat.add

theorem LegitNotation : (⟦True⟧) := trivial
