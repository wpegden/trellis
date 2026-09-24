-- [TABLET NODE: UsesOwnAuxAxiom]
import Tablet.Preamble
import Tablet.AxiomForge

/-- Fix A no-hiding guard (unapproved axiom INSIDE an own aux): the proof
    binds an own `let rec axReachable` whose BODY references the project
    axiom `AxiomForgeEq1` (declared by the `AxiomForge` fixture node).
    The own aux is transparent-walked (Fix A), but the walk must still
    surface the axiom in `kernel_axioms` — in BOTH the primary and the
    axcheck collectors — so the Rust approved-axiom policy still sees it.
    An own-aux filter that dropped the aux's body would be a soundness
    hole (an unapproved axiom smuggled inside a `let rec`). -/
theorem UsesOwnAuxAxiom : 2 + 2 = 4 :=
  let rec axReachable (n : Nat) : 2 + 2 = 4 :=
    match n with
    | 0 => AxiomForgeEq1
    | m + 1 => axReachable m
  axReachable 1
