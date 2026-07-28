-- [TABLET NODE: NamespacedStrictThm]
import Tablet.Preamble

/- NAMESPACED-DEP FIX fixture (PV-shape theorem reached via a STRICT path).
    Namespaced like the others (decl `crate_ns.NamespacedStrictThm`, module
    `Tablet.NamespacedStrictThm`). `NamespacedDef` references this theorem in
    its `def` VALUE, so a consumer that uses `NamespacedDef` reaches this
    theorem through the def-body `.strict` walk — landing it in the consumer's
    `strict_theorem_deps` (not `boundary_theorems`). The emitted dep `name`
    must be the bare node id `NamespacedStrictThm`. Sorry-free. -/
namespace crate_ns

theorem NamespacedStrictThm : True := trivial

end crate_ns
