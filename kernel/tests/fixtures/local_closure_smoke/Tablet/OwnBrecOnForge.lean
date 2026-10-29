-- [TABLET NODE: OwnBrecOnForge]
import Tablet.Preamble

/-- FIX B no-hiding fixture: an OWN-MODULE forgery of a recursor-family
    name. `OwnBrecOnForge` is non-recursive, so Lean generates no
    `brecOn` for it and the hand-authored `theorem OwnBrecOnForge.brecOn`
    below compiles without collision. All three conjuncts of
    `isRecursorFamilyRealization` hold for it (suffix, inductive parent,
    SAME module), so the consumer's probe classifies it as a generated
    artifact and transparent-walks it — which must still WALK its value,
    surfacing the `sorryAx` inside in `kernel_axioms`. Nothing hides
    through the transparent walk. -/
inductive OwnBrecOnForge : Prop where
  | intro : OwnBrecOnForge

/-- The own-module forgery: carries `sorry` so the no-hiding guard has a
    poisoned value to surface. -/
theorem OwnBrecOnForge.brecOn : False := sorry
