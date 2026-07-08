-- [TABLET NODE: ForgeUnderscore]
import Tablet.Preamble

/-- FIX 2 surface (underscore): `protected theorem ForgeUnderscore._helper`.
    The `_`-prefixed family has NO environment provenance signal (genuine
    `_sunfold` and authored `_helper` are indistinguishable in the env), so
    only the authorship (source-parse) check catches it. -/
protected theorem ForgeUnderscore._helper : 2 + 2 = 4 := by decide

theorem ForgeUnderscore : True := trivial
