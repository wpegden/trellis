-- [TABLET NODE: StructFields]
import Tablet.Preamble

/-- Structure node for the generated-member local-closure fix.

    Its field `foo` is an auto-generated projection function
    `StructFields.foo` whose name is arbitrary (user-chosen). The
    local-closure collector must transparent-walk that projection (it is
    part of the structure's principal declaration per `FILESPEC.md`) and
    record the dependency under the structure node `StructFields`, not
    reject `StructFields.foo` as a private auxiliary. -/
structure StructFields where
  foo : Prop
