-- [TABLET NODE: ClassMethod]
import Tablet.Preamble

/-- Class node. Its method `op` is an auto-generated projection
    function `ClassMethod.op` flagged `fromClass` in the environment's
    projection-info. The local-closure collector must transparent-walk it
    (it is part of the class's principal declaration) and record the
    dependency under node `ClassMethod`. -/
class ClassMethod (α : Type) where
  op : α → α
