-- [TABLET NODE: NamespacedThm]
import Tablet.Preamble

/- NAMESPACED-DEP FIX fixture (PV-shape leaf theorem). Like `NamespacedRoot`,
    this opens a crate `namespace` in the FREE REGION above the
    `-- [TABLET NODE: …]` marker, so the on-disk decl is
    `crate_ns.NamespacedThm` while the module stays `Tablet.NamespacedThm`.
    A consumer that references it via its proof body records it as a
    `boundary_theorem`; the emitted dep `name` must be the BARE node id
    `NamespacedThm` (its `Tablet.`-stripped module suffix), NOT the namespaced
    `crate_ns.NamespacedThm`, so the kernel's Patch C-K present-node check
    (keyed by bare node ids) accepts it. Sorry-free. -/
namespace crate_ns

theorem NamespacedThm : True := trivial

end crate_ns
