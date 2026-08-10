-- [TABLET NODE: LoopHost]
import Tablet.Preamble

/- DEFINITION-DEP MODULE-ATTRIBUTION fixture (category (b): Aeneas-shape
    co-generated loop helper). Mirrors how Aeneas co-emits extra `_loop`
    definitions INSIDE a model node's own module — e.g. `left_shift_loop0`,
    `parse_decimal_seq_loop0` — alongside the node's principal declaration.

    This node's module is `Tablet.LoopHost` (node id `LoopHost`, a present
    node). It declares its PRINCIPAL decl `crate_ns.LoopHost` AND a non-principal
    co-generated helper `crate_ns.LoopHost_loop0` living in the SAME module. The
    helper is NOT name-shaped as a generated artifact (`LoopHost_loop0` is not
    `_`-prefixed nor an `eq_`/`match_`/`proof_` family), so the closure walk
    does NOT transparent-walk it; and its final component does NOT sanitize to
    the module stem `LoopHost`, so it is non-principal.

    A consumer whose DEFINITION references the helper records it as a
    `strict_definition_dep`. Pre-fix the probe emitted the raw dotted name
    `crate_ns.LoopHost_loop0`, which Patch C-K fail-closed. Post-fix
    `depDefKeyName` attributes it to its declaring-module node id `LoopHost`.

    `resolveRoot` is unaffected: only `crate_ns.LoopHost` matches the node stem
    `LoopHost`, so the root resolves uniquely (no ambiguity from the helper). -/
namespace crate_ns

def LoopHost_loop0 (n : Nat) : Nat := n + 1

def LoopHost : Nat := LoopHost_loop0 0

end crate_ns
