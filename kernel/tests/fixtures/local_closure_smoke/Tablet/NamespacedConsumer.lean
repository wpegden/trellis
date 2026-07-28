-- [TABLET NODE: NamespacedConsumer]
import Tablet.Preamble
import Tablet.NamespacedThm
import Tablet.NamespacedDef

/- NAMESPACED-DEP FIX fixture (PV-shape consumer). This is the end-to-end
    case the second fix targets: a NAMESPACED node that depends on OTHER
    NAMESPACED nodes. Aeneas-shape: a crate `namespace` opened in the free
    region above the marker, so every decl here (and in the deps) is
    `crate_ns.<Node>` on disk while the modules stay `Tablet.<Node>`.

    The proof references:
      * `NamespacedThm`  — a Tablet theorem reached via the proof body
        (`proofMayAssumeTheorems` mode) ⇒ recorded as a `boundary_theorem`;
      * `NamespacedDef`  — a Tablet `def` ⇒ recorded as a
        `strict_definition_dep`, and its body in turn reaches
        `NamespacedStrictThm` under `.strict` ⇒ a `strict_theorem_dep`.

    So all three cross-node dep maps are non-empty. Before the second fix the
    probe emitted the namespaced Lean `Name`s (`crate_ns.NamespacedThm`, …),
    which have no `Tablet.` prefix for the kernel to strip and so fail the
    Patch C-K present-node check (keyed by bare node ids). After the fix each
    dep `name` is its `Tablet.`-stripped MODULE SUFFIX — the bare node id
    (`NamespacedThm`, `NamespacedDef`, `NamespacedStrictThm`) — which the
    kernel maps to a ratified `present_node`. Sorry-free. -/
namespace crate_ns

theorem NamespacedConsumer : True :=
  -- Force both deps into the closure: apply a constant function (whose body
  -- references the boundary theorem) to the def value.
  (fun (_ : True) => NamespacedThm) NamespacedDef

end crate_ns
