-- [TABLET NODE: UsesOwnerToCtorIdx]
import Tablet.Preamble
import Tablet.OwnerToCtorIdxAux

/-- Consumer whose proof closure references the hand-authored
    `Owner.toCtorIdx` (see `OwnerToCtorIdxAux.lean`). Mirrors the soundness
    audit's fail-OPEN repro: with a fixed-suffix classifier the probe
    returned `status: ok` with EMPTY dep lists because the private auxiliary
    was transparent-walked away, so the Rust validator never saw a key to
    reject. The collector must RECORD `Owner.toCtorIdx` as a dependency key
    (it is a `thmInfo`, not a generated member of `Owner`) so the C-K guard
    can reject it as a private auxiliary of node `Owner`. -/
theorem UsesOwnerToCtorIdx : True := Owner.toCtorIdx
