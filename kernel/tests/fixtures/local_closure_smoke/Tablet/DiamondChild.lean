-- [TABLET NODE: DiamondChild]
import Tablet.Preamble

/-- Diamond-inheritance structure node. `Top` is a common grandparent;
    `Left` and `Right` both extend it; `DiamondChild` extends both. Because
    `Top` cannot be a subobject of `DiamondChild` along both paths, Lean
    generates an auxiliary parent projection `DiamondChild.toTop` (the
    non-subobject parent coercion), recorded under
    `Environment.getAuxParentProjectionInfo?`. The local-closure collector
    must transparent-walk that coercion and record the dependency under the
    structure node `DiamondChild`. -/
structure Top where
  a : Nat

structure Left extends Top where
  b : Nat

structure Right extends Top where
  c : Nat

structure DiamondChild extends Left, Right where
  d : Nat
