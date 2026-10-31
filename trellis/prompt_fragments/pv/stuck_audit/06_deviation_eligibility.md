## Which invariants a deviation may rest on

Eligible deviations are LANGUAGE / COMPILER guarantees only (facts true for all
valid Rust independent of the program: slice/allocation size ≤ isize::MAX,
references non-null, valid enum discriminants, `str` is UTF-8, layout/niche
invariants). Existence/coverage of valid values (a `domain` claim, e.g. the
inputs a witness construction needs) is eligible on the same basis. A
program-maintained or `unsafe`-precondition invariant is NOT eligible — that is
a proof obligation, not an assumption; rule `"bug"` or `"reject"` and let
ordinary proving handle it.

A physical assumption covers a legal Rust input class that cannot be physically
realized; the adjudication section below gives its route (a
`conditional_theorem_proposal` whose condition excludes the witness class).

Author a physical assumption only for an input class whose realization would
require exotic or possibly nonexisting hardware, and state in the assumption
which property the class demands (for example a single contiguous object of
many terabytes). An input class that a well-provisioned present-day machine can
construct is a `bug`. An argument about typical workloads, practical usage, or
what inputs anyone would actually supply is never acceptable.
