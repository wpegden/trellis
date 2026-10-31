-- [TABLET NODE: UsesDefParentBrecOn]
import Tablet.Preamble
import Tablet.DefParentBrecOn

/-- FIX B fail-closed consumer: depends on `DefParentBrecOn.brecOn`, a
    recursor-family-suffixed theorem whose parent is a `def`, not an
    inductive. `isRecursorFamilyRealization`'s inductive-parent conjunct
    must reject it, so the probe records the dotted dep key and the
    kernel's Patch C-K guard rejects it fail-closed. -/
theorem UsesDefParentBrecOn : True := DefParentBrecOn.brecOn
