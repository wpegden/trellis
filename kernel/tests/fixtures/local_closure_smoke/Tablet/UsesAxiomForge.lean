-- [TABLET NODE: UsesAxiomForge]
import Tablet.Preamble
import Tablet.AxiomForge

/-- Consumer of the reserved-shaped axiom `AxiomForge.eq_1`. After FIX 1
    the probe on this consumer must surface `AxiomForge.eq_1` in
    `kernel_axioms` (proving the transparent-walk no longer drops it). -/
theorem UsesAxiomForge : 2 + 2 = 4 := AxiomForge.eq_1
