-- [TABLET NODE: UsesMacroEqForge]
import Tablet.Preamble
import Tablet.MacroEqForge

/-- Consumer of the macro-synthesized reserved-shaped declaration
    `MacroEqForge.eq_1`. This is the downstream half of the FIX 3 residual:
    the cross-node dependency is on a constant whose name was synthesized by
    the owner's `macro` command. The residual is closed at the OWNER
    (`MacroEqForge` is rejected by the scan-only gate for defining a `macro`
    command), so this consumer never gets a clean owner to import. The
    fixture builds (the synthesized constant is a true Prop) to exercise the
    consumer path. -/
theorem UsesMacroEqForge : 17 * 17 = 289 := MacroEqForge.eq_1
