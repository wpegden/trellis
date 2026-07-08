-- [TABLET NODE: ForgeNamespace]
import Tablet.Preamble

/-- FIX 2 surface (namespace): the reserved name is placed via a
    `namespace ForgeNamespace … end` wrapper rather than a dotted declId.
    The line-scanner sees `theorem eq_1` flat; the parse walk extracts the
    declId's final component `eq_1` regardless of the namespace wrapper. -/
theorem ForgeNamespace : True := trivial

namespace ForgeNamespace
theorem eq_1 : 2 + 2 = 4 := by decide
end ForgeNamespace
