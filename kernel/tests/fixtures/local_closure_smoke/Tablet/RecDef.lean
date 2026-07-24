-- [TABLET NODE: RecDef]
import Tablet.Preamble

/-- No-false-positive fixture: a genuine recursive `def`. Lean generates
    real `RecDef._sunfold`, `RecDef.match_1`, `RecDef._eq_*`/`RecDef.eq_*`
    internals — all `isInternalDetail`-shaped, but COMPILER-GENERATED, never
    authored source declarations. The owner-file scan must NOT flag this
    node (the generated internals are not source declaration commands). -/
def RecDef : Nat → Nat
  | 0 => 0
  | (n + 1) => (RecDef n) + 1
