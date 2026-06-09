-- [TABLET NODE: UsesInductive]
import Tablet.Preamble
import Tablet.InductiveNat

/-- Test consumer for Patch C-K Fix 2 (audit MEDIUM-HIGH: inductive
    semantic hash must mix constructor types).

    `UsesInductive` is a theorem whose statement mentions
    `InductiveNat`, so the probe walks `InductiveNat` Strict and emits
    `strict_definition_deps[InductiveNat] = <semantic_hash>`. The hash
    must include the constructor's parameter type. The
    `inductive_constructor_type_change_changes_semantic_hash` test
    mutates `InductiveNat.lean` between two probe invocations and
    asserts the hash changed.

    The statement intentionally references only the inductive `T` and
    quantifies over its values polymorphically, so the consumer compiles
    even when the constructor's parameter type changes from `Nat → T` to
    `Bool → T` (the mutation under test). -/
theorem UsesInductive : ∀ x : InductiveNat, x = x := fun _ => rfl
