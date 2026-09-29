-- Tablet preamble for the local_closure_smoke fixture.
-- Minimal — our test nodes don't need Mathlib.

/- DEFINITION-DEP MODULE-ATTRIBUTION fixture (category (a): Preamble-shared
    structure). Mirrors the dec2flt shape where `BiasedFp` / `DecimalSeq` /
    `Number` are `structure`s declared in the shared `Tablet.Preamble` module
    (node id `Preamble`, a present node) under the crate namespace, so the
    decl name is `crate_ns.PreambleBiasedFp` while the module stays
    `Tablet.Preamble`.

    A consumer whose DEFINITION depends on this struct records it as a
    `strict_definition_dep`. Pre-fix the probe emitted the raw dotted name
    `crate_ns.PreambleBiasedFp` (its final component does NOT sanitize to the
    module stem `Preamble`, so the principal-only `tabletNodeId?` returned
    `none`), which the kernel's Patch C-K present-node validation fail-closed.
    Post-fix `depDefKeyName` maps it to its declaring-module node id `Preamble`
    (a present node), with no principal gate. -/
namespace crate_ns

structure PreambleBiasedFp where
  mantissa : Nat
  exponent : Int

end crate_ns
