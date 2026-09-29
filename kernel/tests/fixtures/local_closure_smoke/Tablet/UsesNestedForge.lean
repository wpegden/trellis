-- [TABLET NODE: UsesNestedForge]
import Tablet.Preamble
import Tablet.Nested_Forge

/- DEEPLY-NAMESPACED-FORGE consumer (the nested-depth soundness assertion).
    Namespaced (PV shape): decl `crate_ns.UsesNestedForge` in module
    `Tablet.UsesNestedForge`. Its proof closure pulls in the namespaced private
    auxiliary `crate_ns.Nested.Forge.realAux` declared in the DIFFERENT node
    `Nested_Forge`'s module.

    Exact membership, rather than name resemblance, is authoritative: the
    reached name stays `crate_ns.Nested.Forge.realAux`, and its genuine module
    manifest assigns registered owner `Nested_Forge`. -/
namespace crate_ns

open crate_ns in
theorem UsesNestedForge : True := Nested.Forge.realAux

end crate_ns
