-- [TABLET NODE: EnumColor]
import Tablet.Preamble

/-- Enum-style inductive node whose constructors have arbitrary names
    (`red`, not `mk`). The constructor `EnumColor.red` is an auto-generated
    member; the local-closure collector recognizes it via the
    environment's constructor predicate, NOT a name suffix. This proves the
    generated-member fix does not rely on a recognizable `mk`/`rec`/etc.
    name. -/
inductive EnumColor where
  | red
  | green
