-- [TABLET NODE: ForgeAfterNotation]
import Tablet.Preamble
import Tablet.NotationLib

/-- FIX 2 robustness fixture (codex "additional risk"): a declaration that
    USES imported notation (`myparens%`, `⟪ … ⟫`) in its body, FOLLOWED by a
    forged reserved-shaped auxiliary. Under the `--scan-only` `Init`-only
    parse, the imported notation tokens are unknown, so the first command
    parse-errors. The forge `eq_1` must STILL be harvested from the later
    command and rejected. If `parseCommand` error-recovery failed to advance
    past the unparseable command, the forge would evade the scan. -/
theorem ForgeAfterNotation : True := ⟪ trivial ⟫

protected theorem ForgeAfterNotation.eq_1 : 2 myparens% + 2 = 4 := by decide
