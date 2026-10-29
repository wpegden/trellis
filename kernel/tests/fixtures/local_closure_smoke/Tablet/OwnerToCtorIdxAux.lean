-- [TABLET NODE: OwnerToCtorIdxAux]
import Tablet.Preamble
import Tablet.Owner

/-- Hand-authored auxiliary whose name collides with the compiler's
    constructor-index helper suffix `toCtorIdx`, in the namespace of the
    structure `Owner`. Lean does NOT generate `toCtorIdx` for a structure
    owner (only for enum-style inductives with several constructors), so
    this `theorem` compiles unblocked — there is no name collision. It is a
    `thmInfo`, NOT a generated member of `Owner`, so a consumer depending on
    it must still record `Owner.toCtorIdx` as a dependency key and the Rust
    C-K guard must reject it as a private auxiliary of node `Owner`.

    This is the soundness regression fixture: an earlier fixed-suffix
    classifier hid this hand-written aux as if it were generated, returning
    `status: ok` with empty dep lists (fail-OPEN). The collector must NOT
    transparent-walk it. -/
theorem Owner.toCtorIdx : True := trivial
