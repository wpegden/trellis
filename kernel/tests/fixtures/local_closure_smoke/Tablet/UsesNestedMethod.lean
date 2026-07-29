-- [TABLET NODE: UsesNestedMethod]
import Tablet.Preamble
import Tablet.Nested_method

/- DEEPLY-NAMESPACED-DEP consumer (the dec2flt-shape end-to-end assertion).
    Namespaced (PV shape): decl `crate_ns.UsesNestedMethod` in module
    `Tablet.UsesNestedMethod`. Its proof references the def
    `crate_ns.Nested.method` (node `Nested_method`, whose FILESPEC stem
    flattens its below-namespace path `Nested.method`), and through that def's
    body reaches the theorem `NamespacedStrictThm`.

    Before the fix the probe emitted the full namespaced dep `Name`
    (`crate_ns.Nested.method`) — no `Tablet.` prefix, not a bare present-node
    id — which the kernel's Patch C-K present-node validation rejected. After
    the fix `declMatchesStem` maps it to the bare node id `Nested_method`
    (`Nested.method` -> `Nested_method`), the kernel present-node key. This is
    the dec2flt `dec2flt_biased_exact_correct_of_round_invariant` shape (deps
    like `BiasedFp.Insts.X.eq` -> `BiasedFp_Insts_X_eq`). Sorry-free. -/
namespace crate_ns

theorem UsesNestedMethod : True := Nested.method

end crate_ns
