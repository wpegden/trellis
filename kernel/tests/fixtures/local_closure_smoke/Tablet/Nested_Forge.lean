-- [TABLET NODE: Nested_Forge]
import Tablet.Preamble

/- DEEPLY-NAMESPACED-FORGE fixture (the SOUNDNESS case at NESTED depth). Like
    `NamespacedForgeAux` but with a MULTI-segment below-namespace path so the
    FILESPEC stem flattens dots to underscores. The PRINCIPAL declaration is
    `crate_ns.Nested.Forge` in module `Tablet.Nested_Forge` (stem
    `Nested_Forge` = `Nested.Forge` with `.`→`_`). It ALSO authors a private
    auxiliary `crate_ns.Nested.Forge.realAux` — a hand-written `thmInfo`, NOT a
    generated member.

    The aux's trailing component runs (`realAux`, `Forge_realAux`,
    `Nested_Forge_realAux`, …) NONE equal the stem `Nested_Forge`, so
    `declMatchesStem` (the FILESPEC name-parity test) must NOT collapse it to
    the bare present-node id `Nested_Forge`. It stays a dotted `Name`, which the
    kernel's Patch C-K guard rejects fail-closed. The principal `Nested.Forge`
    DOES match the stem (run `Nested.Forge`), so it resolves as the root and
    maps as a dep — exactly the principal/auxiliary split the guard must keep at
    arbitrary namespace depth. -/
namespace crate_ns

theorem Nested.Forge : True := trivial

theorem Nested.Forge.realAux : True := trivial

end crate_ns
