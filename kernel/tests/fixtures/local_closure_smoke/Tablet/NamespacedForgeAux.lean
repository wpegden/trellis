-- [TABLET NODE: NamespacedForgeAux]
import Tablet.Preamble

/- NAMESPACED-FORGE fixture (the SOUNDNESS case the dep-fix's
    principal-declaration guard protects). This node opens a crate
    `namespace` (Aeneas/PV shape) in the free region above the marker, so its
    PRINCIPAL declaration is `crate_ns.NamespacedForgeAux` in module
    `Tablet.NamespacedForgeAux`. It ALSO authors a private auxiliary
    `crate_ns.NamespacedForgeAux.realAux` — a hand-written `thmInfo`, NOT a
    generated member — under the SAME crate namespace.

    The aux's on-disk `Name` (`crate_ns.NamespacedForgeAux.realAux`) has FINAL
    component `realAux`, which differs from the node-id final component
    `NamespacedForgeAux` (the module suffix). So the dep-fix's `tabletNodeId?`
    principal-declaration guard must NOT collapse it to the bare present-node
    id `NamespacedForgeAux`; it must stay a dotted `Name`, which the kernel's
    Patch C-K guard then rejects (fail-closed) when a consumer depends on it.

    Without that guard, a cross-node reference to this aux would be silently
    re-keyed onto the principal node id `NamespacedForgeAux` (a present node),
    bypassing the private-auxiliary rejection entirely — a soundness hole. -/
namespace crate_ns

theorem NamespacedForgeAux : True := trivial

theorem NamespacedForgeAux.realAux : True := trivial

end crate_ns
