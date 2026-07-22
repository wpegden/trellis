-- [TABLET NODE: UsesDiamondParent]
import Tablet.Preamble
import Tablet.DiamondChild

/-- Consumer whose proof closure references the `extends`-diamond
    non-subobject parent coercion `DiamondChild.toTop`. Under the
    generated-member fix the probe recognizes it via
    `Environment.getAuxParentProjectionInfo?`, transparent-walks it, and
    resolves the dependency to node `DiamondChild` rather than rejecting
    `DiamondChild.toTop` as a private auxiliary. -/
theorem UsesDiamondParent (x : DiamondChild) : (x.toTop).a = x.toLeft.toTop.a := rfl
