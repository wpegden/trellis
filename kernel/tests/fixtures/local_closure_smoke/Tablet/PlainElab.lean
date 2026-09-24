-- [TABLET NODE: PlainElab]
import Lean
import Tablet.Preamble

/-- FIX 3: a node defining a term `elab` command. Like `PlainMacro`, the
    expansion here is harmless, but `elab` is in the banned command family
    (an `elab` can run arbitrary `CommandElabM`/`TermElabM` and synthesize
    declarations), so the node is rejected. -/
theorem PlainElab : True := trivial

-- A term-level elaborator returning the constant `True.intro`. Harmless,
-- but `elab` is in the banned command family (it can run arbitrary
-- elaboration and synthesize declarations), so the node is rejected.
elab "myElabTriv" : term => return Lean.mkConst ``True.intro
