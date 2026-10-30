-- [TABLET NODE: UsesNamespacedForgeAux]
import Tablet.Preamble
import Tablet.NamespacedForgeAux

/- NAMESPACED-FORGE consumer (the soundness assertion). Namespaced (PV shape):
    decl `crate_ns.UsesNamespacedForgeAux` in module
    `Tablet.UsesNamespacedForgeAux`. Its proof closure pulls in the namespaced
    private auxiliary `crate_ns.NamespacedForgeAux.realAux` declared in the
    DIFFERENT node `NamespacedForgeAux`'s module.

    The probe must record that aux as a cross-node dep keyed by its DOTTED
    `Name` (final component `realAux` ≠ the module-suffix node-id final
    component `NamespacedForgeAux`, so the principal-declaration guard leaves
    it un-collapsed). The kernel's Patch C-K present-node validation then
    rejects the probe (fail-closed): the dotted aux key is NOT a ratified
    present node. The dep-fix must NOT silently re-key it onto the bare,
    present node id `NamespacedForgeAux`. -/
namespace crate_ns

open crate_ns in
theorem UsesNamespacedForgeAux : True := NamespacedForgeAux.realAux

end crate_ns
