-- [TABLET NODE: UsesEnumCtor]
import Tablet.Preamble
import Tablet.EnumColor

/-- Consumer whose proof closure references the constructor
    `EnumColor.red` (an arbitrary, non-`mk` constructor name). Under the
    generated-member fix the probe resolves this to node `EnumColor` and
    does NOT push a private-auxiliary rejection. -/
theorem UsesEnumCtor : EnumColor.red = EnumColor.red := rfl
