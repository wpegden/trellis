-- [TABLET NODE: UsesInstanceValue]
import Tablet.Preamble
import Tablet.InstanceValue

/-- Consumer of the instance `InstanceValue`. Its definition value
    references the instance directly, so the instance's value enters this
    node's correspondence-fingerprint closure. The Part 2 fingerprint test
    mutates the instance's `op` body and asserts this node's fingerprint
    changes. -/
def UsesInstanceValue : Nat → Nat := fun n => ClassMethod.op (self := InstanceValue) n
