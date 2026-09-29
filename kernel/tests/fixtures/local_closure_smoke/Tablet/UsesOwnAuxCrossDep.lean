-- [TABLET NODE: UsesOwnAuxCrossDep]
import Tablet.Preamble
import Tablet.Helper

/-- Fix A no-hiding guard (cross-node dep INSIDE an own aux): the proof
    binds an own `let rec bridgeReachable` whose BODY references the
    imported boundary theorem `Helper` (another node, carries `sorryAx`
    in its value). The own aux itself must be transparent-walked (Fix A),
    but the transparent walk must still WALK its value, so `Helper` IS
    recorded as a boundary theorem — the aux filter must never hide a
    genuine cross-node reference. The boundary cut at `Helper` still
    applies: no `sorryAx` leaks into `kernel_axioms`. -/
theorem UsesOwnAuxCrossDep : True :=
  let rec bridgeReachable (n : Nat) : True :=
    match n with
    | 0 => Helper
    | m + 1 => bridgeReachable m
  bridgeReachable 2
