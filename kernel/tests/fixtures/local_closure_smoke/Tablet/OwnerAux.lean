-- [TABLET NODE: OwnerAux]
import Tablet.Preamble
import Tablet.Owner

/-- A hand-authored theorem in the `Owner` namespace. It is NOT a
    generated member of the structure `Owner` (it is a `thmInfo`), so a
    consumer depending on `Owner.realAux` must still be rejected as a
    private-auxiliary dependency — the over-admission guard the
    local-closure check was built for. -/
theorem Owner.realAux : True := trivial
