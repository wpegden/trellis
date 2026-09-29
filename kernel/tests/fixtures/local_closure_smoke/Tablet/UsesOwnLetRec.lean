-- [TABLET NODE: UsesOwnLetRec]
import Tablet.Preamble

/-- Fix A regression (own-node `let rec` aux must be transparent-walked):
    the proof binds a USER-NAMED, self-recursive `let rec prefixReachable`
    producing a proof. Lean lifts it to the real constant
    `UsesOwnLetRec.prefixReachable` in this node's OWN module. It is an
    artifact of this node's own elaboration — NOT a cross-node reference —
    but its name is not reserved-shaped, so the name-shape-based
    `isTabletGeneratedArtifact` cannot see it. Before Fix A the probe
    recorded it as a dotted dep key (`UsesOwnLetRec.prefixReachable`),
    which the kernel's Patch C-K present-node validation fail-closed on →
    spurious `internal_error`. After Fix A (`isOwnNodeAux`) both
    collectors transparent-walk it: status ok, no `.prefixReachable` dep
    key, axioms ⊆ canonical four, dual collectors agreed. -/
theorem UsesOwnLetRec : True :=
  let rec prefixReachable (n : Nat) : True :=
    match n with
    | 0 => trivial
    | m + 1 => prefixReachable m
  prefixReachable 3
