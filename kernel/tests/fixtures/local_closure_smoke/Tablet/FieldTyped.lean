-- [TABLET NODE: FieldTyped]
import Tablet.Preamble

/-- Structure whose field TYPE is mutated by the Part 2 fingerprint
    regression test (`val : Nat` → `val : Bool`). A field-type change is a
    semantic change to the structure and must move the correspondence
    fingerprint of any consumer that depends on the structure. -/
structure FieldTyped where
  val : Nat
