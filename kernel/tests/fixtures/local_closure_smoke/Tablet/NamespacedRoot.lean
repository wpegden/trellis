-- [TABLET NODE: NamespacedRoot]
import Tablet.Preamble

/- Namespaced-root regression fixture (PV-shape). Aeneas-extracted PV nodes
    open a crate `namespace` in the FREE REGION above the `-- [TABLET NODE: …]`
    marker, so the node-named theorem lands at `<crate>.<NodeName>` even though
    the module is still `Tablet.<NodeName>` and lake compiled it cleanly. The
    probe must resolve the root by the node's namespaced on-disk declaration
    (unique same-final-name decl in the node's own module), NOT by the bare
    node name — otherwise it spuriously reports `missing_declaration`.

    The theorem is sorry-free (kernel_axioms ⊆ canonical four), so once
    resolved it must probe `ok`. -/
namespace crate_ns

theorem NamespacedRoot : True := trivial

end crate_ns
