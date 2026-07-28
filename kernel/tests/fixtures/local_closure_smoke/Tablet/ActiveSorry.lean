-- [TABLET NODE: ActiveSorry]
import Tablet.Preamble

/-- The active proof itself contains `sorry`. The probe must report
    `kernel_axioms ∋ sorryAx`. The Patch B gate should reject this
    node when `must_close_active = true`; Patch A is observation
    only, so the wrapper just surfaces the axiom set. -/
theorem ActiveSorry : True := by sorry
