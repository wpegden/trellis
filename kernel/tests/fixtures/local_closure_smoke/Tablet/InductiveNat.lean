-- [TABLET NODE: InductiveNat]
import Tablet.Preamble

/-- Test inductive fixture for Patch C-K Fix 2 (audit MEDIUM-HIGH:
    inductive semantic hash must mix constructor types).

    The `mk : Nat → InductiveNat` constructor's parameter type is
    `Nat`. The sibling fixture `InductiveBool` has the same name and
    constructor name but `mk : Bool → InductiveBool`. The probe's
    `strict_definition_deps` hash for these two inductives MUST
    differ — under the pre-fix hashing rule (type + ctor names only),
    they hashed identically because `v.type` is `Type` for both and
    the ctor name `mk` is the same. -/
inductive InductiveNat where
  | mk : Nat → InductiveNat
