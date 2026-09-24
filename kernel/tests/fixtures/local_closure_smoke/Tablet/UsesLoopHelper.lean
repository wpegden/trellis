-- [TABLET NODE: UsesLoopHelper]
import Tablet.Preamble
import Tablet.LoopHost

/- DEFINITION-DEP MODULE-ATTRIBUTION consumer (category (b)). A namespaced
    (PV-shape) `def` whose VALUE references `crate_ns.LoopHost_loop0` — the
    non-principal co-generated loop helper living inside the `Tablet.LoopHost`
    module (node id `LoopHost`, a present node). The helper (a `defnInfo`, NOT a
    theorem) is recorded as a `strict_definition_dep`.

    Pre-fix the probe emitted the raw dotted dep name `crate_ns.LoopHost_loop0`,
    which `validate_probe_present_nodes` fail-closed. Post-fix `depDefKeyName`
    attributes it to its declaring-module node id `LoopHost`, which the kernel
    maps to a ratified present node. Sorry-free. -/
namespace crate_ns

def UsesLoopHelper : Nat := LoopHost_loop0 41

end crate_ns
