-- [TABLET NODE: StructForgeAux]
import Tablet.Preamble

/-- FIX 2 coverage (`structure` definition node): a `structure` principal
    declaration that ALSO authors a reserved-shaped auxiliary declaration
    command `StructForgeAux._helper`. The structure's own generated members
    (the `mk` constructor, field projections) are legitimately part of the
    principal and must NOT be flagged — but a hand-authored reserved-shaped
    auxiliary alongside it is an unregistered private auxiliary and must be
    rejected. The `protected` modifier defeats the first-token line-scanner,
    so the authoritative parse is what catches it. The owner-file scan runs
    for every node kind, structure definitions included. -/
structure StructForgeAux where
  val : Nat

protected def StructForgeAux._helper : Nat := 0
