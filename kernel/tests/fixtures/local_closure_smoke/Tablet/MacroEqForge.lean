-- [TABLET NODE: MacroEqForge]
import Tablet.Preamble

/-- FIX 3 forge (the macro-generated-name residual): the owner file authors
    a *command macro* that EXPANDS to a reserved-shaped top-level
    declaration. The synthesized constant `MacroEqForge.eq_1` never appears
    literally in source as a `declId` (the macro builds the identifier with
    `mkIdent`, defeating hygiene so the *literal* `MacroEqForge.eq_1` is
    created), so the FIX 2 syntactic authored-name scan cannot see the
    string `eq_1`, and the closure probe would transparent-walk the
    synthesized constant as an "internal detail" — the exact review-integrity
    bypass. FIX 3 closes this by rejecting the OWNER at its own acceptance
    for *defining a `macro` command at all* (the ban is on the command
    family, not on what it happens to emit). The file elaborates cleanly so
    the fixture `lake build`s; the point is that the scan rejects it. -/
theorem MacroEqForge : 2 + 2 = 4 := by decide

-- A command macro emitting a reserved-shaped declaration. `mkIdent` builds
-- the unhygienic literal name `MacroEqForge.eq_1`, so the synthesized
-- constant's name is exactly the reserved-shaped one — yet the string
-- `eq_1` never appears as a written `declId` anywhere in source.
open Lean in
macro "declare_eq1" : command =>
  `(theorem $(mkIdent `MacroEqForge.eq_1) : 17 * 17 = 289 := by decide)

declare_eq1
