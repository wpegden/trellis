-- [TABLET NODE: InstanceValue]
import Tablet.Preamble
import Tablet.ClassMethod

/-- An instance whose VALUE is mutated by the Part 2 fingerprint
    regression test. `ClassMethod.op` is the identity here; the test
    rewrites the field-value body and asserts the correspondence
    fingerprint of a consumer changes. An instance elaborates to a `def`,
    so its `value` is part of its meaning and must enter the fingerprint. -/
instance InstanceValue : ClassMethod Nat where
  op := fun n => n
