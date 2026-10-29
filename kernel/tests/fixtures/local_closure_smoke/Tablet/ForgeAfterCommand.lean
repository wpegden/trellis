-- [TABLET NODE: ForgeAfterCommand]
import Tablet.Preamble
import Tablet.CommandLib

/-- FIX 2 robustness fixture (codex "additional risk"): a custom COMMAND
    macro (`declare_trivial`) is invoked as a top-level command BEFORE a
    forged reserved-shaped auxiliary. Under the `--scan-only` `Init`-only
    parse, `declare_trivial` is an unknown command token, so that command
    parse-errors. The forge `eq_1` in the following command must STILL be
    harvested and rejected. This is the strongest form of the concern: an
    unparseable *command head* (not merely an unknown token inside an
    otherwise well-formed command) preceding the forge. -/
theorem ForgeAfterCommand : True := trivial

declare_trivial generatedByMacro

protected theorem ForgeAfterCommand.eq_1 : 2 + 2 = 4 := by decide
