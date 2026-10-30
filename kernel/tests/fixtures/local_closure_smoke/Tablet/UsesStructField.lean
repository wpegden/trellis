-- [TABLET NODE: UsesStructField]
import Tablet.Preamble
import Tablet.StructFields

/-- Consumer whose proof closure references the structure field
    projection `StructFields.foo`. Under the generated-member fix the
    probe resolves this dependency to node `StructFields` (a registered
    principal) and does NOT push a private-auxiliary rejection /
    `internal_error`. -/
theorem UsesStructField (s : StructFields) (h : s.foo) : StructFields.foo s := h
