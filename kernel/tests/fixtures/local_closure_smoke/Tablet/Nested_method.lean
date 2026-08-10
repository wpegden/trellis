-- [TABLET NODE: Nested_method]
import Tablet.Preamble
import Tablet.NamespacedStrictThm

/- DEEPLY-NAMESPACED-DEP regression fixture (the dec2flt-shape case the prior
    dep-fix MISSED). The node's on-disk file stem / module is the
    FILESPEC-sanitized single component `Nested_method` (dots are illegal in a
    stem, so the below-namespace path `Nested.method` flattens with `.`→`_`),
    while its PRINCIPAL declaration is `crate_ns.Nested.method` — a MULTI-segment
    below-namespace path. This is exactly the dec2flt shape
    (`crate.BiasedFp.Insts.X.eq` in module `Tablet.BiasedFp_Insts_X_eq`).

    The decl's FINAL component (`method`) does NOT equal the module suffix
    (`Nested_method`), so the prior `tabletNodeId?` principal-declaration guard
    (final component == module-suffix final) returned `none` and emitted the
    full namespaced `Name` (`crate_ns.Nested.method`), which the kernel's Patch
    C-K present-node validation then REJECTED (no `Tablet.` prefix, not a bare
    present-node id) — blocking `formalization_complete` for every dec2flt node
    with such a dep. The same flattening also broke `resolveRoot` (the decl's
    final component `method` ≠ the bare node name `Nested_method`), so the node
    could not even be probed as a ROOT.

    The fixed `declMatchesStem` FILESPEC name-parity test recognizes this as the
    node's principal declaration on BOTH sides: the trailing component run
    `Nested.method`, with `.`→`_`, equals the stem `Nested_method`. As a root it
    resolves; as a dep it emits the bare node id `Nested_method` — the kernel
    present-node key. This def references `NamespacedStrictThm` in its value so a
    consumer records BOTH `Nested_method` (strict_definition_dep) and
    `NamespacedStrictThm` (strict_theorem_dep, reached through the body).
    Sorry-free. -/
namespace crate_ns

def Nested.method : True := NamespacedStrictThm

end crate_ns
