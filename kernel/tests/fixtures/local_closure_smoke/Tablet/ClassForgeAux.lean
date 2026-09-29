-- [TABLET NODE: ClassForgeAux]
import Tablet.Preamble

/-- FIX 2 coverage (`class` definition node): a `class` principal
    declaration that authors a reserved-shaped auxiliary `ClassForgeAux._aux`
    alongside it. The class's generated method projections are legitimately
    part of the principal; the hand-authored reserved-shaped auxiliary is
    not, and is rejected by the universal owner-file scan that runs for the
    `class` keyword path too. -/
class ClassForgeAux (α : Type) where
  op : α → α

protected def ClassForgeAux._aux : Nat := 0
