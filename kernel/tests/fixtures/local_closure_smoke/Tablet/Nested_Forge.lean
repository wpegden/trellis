-- [TABLET NODE: Nested_Forge]
import Tablet.Preamble

/- DEEPLY-NAMESPACED-FORGE fixture (the SOUNDNESS case at NESTED depth). Like
    `NamespacedForgeAux` but with a MULTI-segment below-namespace path so the
    FILESPEC stem flattens dots to underscores. The PRINCIPAL declaration is
    `crate_ns.Nested.Forge` in module `Tablet.Nested_Forge` (stem
    `Nested_Forge` = `Nested.Forge` with `.`→`_`). It ALSO authors a private
    auxiliary `crate_ns.Nested.Forge.realAux` — a hand-written `thmInfo`, NOT a
    generated member.

    The auxiliary's name does not resemble the module stem. That is now
    deliberately irrelevant: the manifest proves it is an exact member of
    module `Tablet.Nested_Forge`, and declaring-module evidence attributes it
    to registered owner `Nested_Forge`. The principal is supplied separately
    by exact FILESPEC registration. -/
namespace crate_ns

theorem Nested.Forge : True := trivial

theorem Nested.Forge.realAux : True := trivial

end crate_ns
