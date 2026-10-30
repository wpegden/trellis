-- [TABLET NODE: NonRecPred]
import Tablet.Preamble

/-- FIX B soundness fixture: a NON-recursive Prop-valued inductive. Lean
    generates NO `brecOn` / `below` for it (`IndPredBelow` and
    `mkAuxConstructions` build those only for recursive inductives), so a
    hand-authored `theorem NonRecPred.brecOn` compiles without a name
    collision. `ForgeBrecOnAux.lean` authors exactly that forgery in a
    DIFFERENT module; the module co-location clause of
    `isRecursorFamilyRealization` must keep it a recorded, fail-closed
    dotted dep key. -/
inductive NonRecPred : Prop where
  | intro : NonRecPred
