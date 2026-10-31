-- [TABLET NODE: UsesNestedMethod]
import Tablet.Preamble
import Tablet.Nested_method

/- DEEPLY-NAMESPACED-DEP consumer (the dec2flt-shape end-to-end assertion).
    Namespaced (PV shape): decl `crate_ns.UsesNestedMethod` in module
    `Tablet.UsesNestedMethod`. Its proof references the def
    `crate_ns.Nested.method` (node `Nested_method`, whose FILESPEC stem
    flattens its below-namespace path `Nested.method`), and through that def's
    body reaches the theorem `NamespacedStrictThm`.

    The probe keeps reached declaration `crate_ns.Nested.method` exactly and
    independently records its declaring module's registered owner
    `Nested_method`. This is the dec2flt
    `dec2flt_biased_exact_correct_of_round_invariant` shape. Sorry-free. -/
namespace crate_ns

theorem UsesNestedMethod : True := Nested.method

end crate_ns
