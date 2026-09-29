-- [TABLET NODE: UsesOwnerAux]
import Tablet.Preamble
import Tablet.OwnerAux

/-- Consumer whose proof closure references the hand-authored
    `Owner.realAux`. The probe records `Owner.realAux` as a dependency key
    (it is not a projection/constructor/aux-recursor), and the Rust C-K
    guard rejects it as a private auxiliary of node `Owner` →
    `internal_error`. This is the over-admission guard for the
    generated-member fix. -/
theorem UsesOwnerAux : True := Owner.realAux
