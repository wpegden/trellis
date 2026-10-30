-- [TABLET NODE: UsesPreambleStruct]
import Tablet.Preamble

/- DEFINITION-DEP MODULE-ATTRIBUTION consumer (category (a)). A namespaced
    (PV-shape) `def` whose VALUE constructs a `crate_ns.PreambleBiasedFp` — the
    struct declared in the shared `Tablet.Preamble` module (node id `Preamble`,
    a present node). The struct (a `defnInfo`/`inductInfo` member, NOT a
    theorem) is recorded as a `strict_definition_dep`.

    Pre-fix the probe emitted the raw dotted dep name `crate_ns.PreambleBiasedFp`
    (final component ≠ module stem `Preamble`, so `tabletNodeId?` returned
    `none`), which `validate_probe_present_nodes` fail-closed. Post-fix
    `depDefKeyName` attributes it to its declaring-module node id `Preamble`,
    which the kernel maps to a ratified present node. Sorry-free. -/
namespace crate_ns

def UsesPreambleStruct : PreambleBiasedFp :=
  { mantissa := 0, exponent := 0 }

end crate_ns
