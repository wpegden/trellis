-- [TABLET NODE: Owner]
import Tablet.Preamble

/-- Structure node for the over-admission guard. Its field projection
    `Owner.field` is generated and MUST be transparent-walked, but a
    separately hand-authored `theorem Owner.realAux` (see `OwnerAux.lean`)
    is NOT generated — it is a `thmInfo` and must still be rejected as a
    private auxiliary when a consumer depends on it. The generated-member
    fix must not over-admit hand-written `Owner.*` declarations. -/
structure Owner where
  field : Prop
