-- [TABLET NODE: UsesFieldTyped]
import Tablet.Preamble
import Tablet.FieldTyped

/-- Consumer of `FieldTyped`. Its statement mentions the structure, so the
    fingerprint walks `FieldTyped` and (via the structure's constructor
    type, which carries the field type) the fingerprint changes when the
    field type changes. The statement quantifies polymorphically so it
    compiles under both `val : Nat` and `val : Bool`. -/
theorem UsesFieldTyped : ∀ x : FieldTyped, x = x := fun _ => rfl
