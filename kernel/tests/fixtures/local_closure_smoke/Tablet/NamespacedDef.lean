-- [TABLET NODE: NamespacedDef]
import Tablet.Preamble
import Tablet.NamespacedStrictThm

/- NAMESPACED-DEP FIX fixture (PV-shape `def`). Namespaced (decl
    `crate_ns.NamespacedDef`, module `Tablet.NamespacedDef`). Its VALUE
    references the namespaced theorem `NamespacedStrictThm`, so a consumer
    that uses this def records BOTH `NamespacedDef` (as a
    `strict_definition_dep`) and `NamespacedStrictThm` (as a
    `strict_theorem_dep`, reached through this def's body under `.strict`).
    Both emitted dep `name`s must be bare node ids. -/
namespace crate_ns

open crate_ns in
def NamespacedDef : True := NamespacedStrictThm

end crate_ns
