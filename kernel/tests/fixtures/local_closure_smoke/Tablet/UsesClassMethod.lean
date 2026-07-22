-- [TABLET NODE: UsesClassMethod]
import Tablet.Preamble
import Tablet.ClassMethod

/-- Consumer whose proof closure references the class method projection
    `ClassMethod.op`. Under the generated-member fix the probe resolves
    this to node `ClassMethod` and does NOT push a private-auxiliary
    rejection. -/
theorem UsesClassMethod {α : Type} [ClassMethod α] (x : α) :
    ClassMethod.op x = ClassMethod.op x := rfl
