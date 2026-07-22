-- [TABLET NODE: UsesNestedForge]
import Tablet.Preamble
import Tablet.Nested_Forge

/- DEEPLY-NAMESPACED-FORGE consumer (the nested-depth soundness assertion).
    Namespaced (PV shape): decl `crate_ns.UsesNestedForge` in module
    `Tablet.UsesNestedForge`. Its proof closure pulls in the namespaced private
    auxiliary `crate_ns.Nested.Forge.realAux` declared in the DIFFERENT node
    `Nested_Forge`'s module.

    No trailing component run of `crate_ns.Nested.Forge.realAux` flattens to the
    stem `Nested_Forge`, so `declMatchesStem` leaves the dep key DOTTED. The
    kernel's Patch C-K present-node validation then rejects the probe
    (fail-closed): the dotted aux key is not a ratified present node. The fix
    must NOT silently re-key it onto the bare present node id `Nested_Forge`
    (the soundness hole the principal-declaration guard closes — now at nested
    namespace depth). -/
namespace crate_ns

open crate_ns in
theorem UsesNestedForge : True := Nested.Forge.realAux

end crate_ns
