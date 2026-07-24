-- [TABLET NODE: LegitAux]
import Tablet.Preamble

/-- No-false-positive fixture: a legitimate non-reserved `protected`
    auxiliary `LegitAux.helper`. Final component `helper` is NOT
    reserved-shaped, so the owner-file scan must allow it (status: ok). The
    principal `LegitAux` (final component = node name) is likewise never
    flagged. -/
protected theorem LegitAux.helper : 2 + 2 = 4 := by decide

theorem LegitAux : True := trivial
