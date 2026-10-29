-- [TABLET NODE: AmbiguousRoot]
import Tablet.Preamble

/- AMBIGUOUS-ROOT fixture (root-fix `ambiguous_declaration` coverage). The
    node-named declaration is NOT top-level (so the fast path misses it) and
    there are TWO non-generated declarations with final component
    `AmbiguousRoot` in this node's own module `Tablet.AmbiguousRoot`, placed
    under two different namespaces opened in the free region above the marker
    (`crate_ns.AmbiguousRoot` and `other_ns.AmbiguousRoot`).

    `resolveRoot` must therefore find MORE THAN ONE same-final-name candidate
    in the node's own module and emit `status:"ambiguous_declaration"` — a
    distinct, safe over-reject, never an arbitrary pick. (A real PV node has a
    single crate namespace, so this ambiguity does not arise in practice; the
    fixture exercises the explicit ambiguity branch.) -/
namespace crate_ns

theorem AmbiguousRoot : True := trivial

end crate_ns

namespace other_ns

theorem AmbiguousRoot : True := trivial

end other_ns
