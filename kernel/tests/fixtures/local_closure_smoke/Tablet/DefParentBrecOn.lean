-- [TABLET NODE: DefParentBrecOn]
import Tablet.Preamble

/-- FIX B fail-closed fixture: a recursor-family SUFFIX under a
    NON-inductive parent. `DefParentBrecOn` is a `def` (a `defnInfo`, not
    an `inductInfo`), so the inductive-parent conjunct of
    `isRecursorFamilyRealization` must reject the hand-authored
    `DefParentBrecOn.brecOn` below even though the suffix matches and the
    two declarations are module-co-located. A consumer's probe must keep
    the dotted dep key recorded (fail-closed). -/
def DefParentBrecOn : Nat := 0

/-- Suffix-shaped auxiliary under the def's namespace. -/
theorem DefParentBrecOn.brecOn : True := trivial
