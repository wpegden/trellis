-- [TABLET NODE: ByElab]
import Lean
import Tablet.Preamble

/- P0 defense-in-depth fixture: `by_elab` is a TERM syntax kind whose
elaborator can reach `Lean.addDecl`. The recursive source scan must reject it
even though the top-level command kind is an ordinary theorem declaration. -/
theorem ByElab : True := by_elab
  return Lean.mkConst ``True.intro
