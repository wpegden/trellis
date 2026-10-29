-- [TABLET NODE: UsesRecDef]
import Tablet.Preamble
import Tablet.RecDef

/-- Consumer of the genuine recursive def `RecDef`. Forces realization of
    `RecDef`'s equation lemmas / `_sunfold` in the closure walk. The probe
    must transparent-walk those generated internals (status: ok), recording
    only the real dependency `RecDef`, and must NOT reject either node. -/
theorem UsesRecDef : RecDef 0 = 0 := by
  simp [RecDef]
