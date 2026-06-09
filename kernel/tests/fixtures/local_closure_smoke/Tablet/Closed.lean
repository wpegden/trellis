-- [TABLET NODE: Closed]
import Tablet.Preamble

/-- A trivially closed node with no Tablet helpers. The probe must
    report `status = ok`, `kernel_axioms ⊆ canonical four`,
    `boundary_theorems = ∅`, and no errors. -/
theorem Closed : True := trivial
