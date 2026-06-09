-- [TABLET NODE: UsesHelper]
import Tablet.Preamble
import Tablet.Helper

/-- Active proof leans on `Helper`. Under the boundary-cut semantics
    of plan §2.2, the probe should treat `Helper` as a boundary
    theorem (recording its `statement_hash`) and walk only its
    `type` (which is `True`, contributing nothing). The result is
    `kernel_axioms ⊆ canonical four` (no `sorryAx` leakage) and
    `boundary_theorems ∋ Helper`. -/
theorem UsesHelper : True := Helper
